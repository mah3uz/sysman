use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use gpui::StyledImage;
use gpui::{
    AnyElement, App, AppContext, ClipboardItem, Context, Div, Entity, FontWeight, Global, Hsla, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, Stateful, StatefulInteractiveElement, Styled, Window, div, img,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::table::{Column, ColumnSort, DataTable, TableDelegate, TableState};
use gpui_component::tooltip::Tooltip;
use gpui_component::{Icon, Selectable, Sizable, Size, WindowExt};
use gpui_kit_assets::IconName;
use schema::Sample;
use schema::cpu::CpuMeta;
use schema::processes::Process;

use crate::charts::core_bars::{CoreBar, core_bars};
use crate::charts::format::{Unit, bytes};
use crate::charts::sparkline::Sparkline;
use crate::charts::trend::{ChartState, TrendChart, YScale, clock};
use crate::data::desktop::IconIndex;
use crate::data::history::{Bucket, History, ProcField, bucket_width, db_path};
use crate::data::live::Live;
use crate::data::process::ProcessFeed;
use crate::data::source::{Combine, Source, top_up};
use crate::screens::dashboard::{Groups, cpu_colour};
use crate::ui::shell::Tab;
use crate::ui::theme::theme;
use crate::ui::widgets::card::card;
use crate::ui::widgets::hairline;
use crate::ui::widgets::panel::caption_label;
use crate::ui::widgets::range::{Range, range_picker, remember, remembered};

const BUCKETS: u32 = 120;
const RELOAD_S: f64 = 60.;
const INSPECTOR_WIDTH: f32 = 380.;
const CHART_HEIGHT: f32 = 110.;
const INDENT: f32 = 14.;
const TRAIL_S: f64 = 30. * 60.;
/// The daemon records processes every 10 s.
const RECORD_INTERVAL_S: f64 = 10.;
const FIELDS: [ProcField; 5] = [
    ProcField::Rss,
    ProcField::Cpu,
    ProcField::Fds,
    ProcField::Read,
    ProcField::Write,
];

/// A process instance: pids are reused, start times disambiguate.
pub type Key = (u32, u64);

pub fn key(p: &Process) -> Key {
    (p.pid, p.start_time)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    Process,
    Memory,
    Cpu,
    Threads,
    Fds,
    User,
    Pid,
}

impl Col {
    const ALL: [Col; 7] = [
        Self::Process,
        Self::Memory,
        Self::Cpu,
        Self::Threads,
        Self::Fds,
        Self::User,
        Self::Pid,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Process => "Process",
            Self::Memory => "Memory (RSS)",
            Self::Cpu => "CPU",
            Self::Threads => "Threads",
            Self::Fds => "FDs",
            Self::User => "User",
            Self::Pid => "PID",
        }
    }

    fn width(self) -> f32 {
        match self {
            Self::Process => 300.,
            Self::Memory => 112.,
            Self::Cpu => 72.,
            Self::Threads => 72.,
            Self::Fds => 64.,
            Self::User => 110.,
            Self::Pid => 76.,
        }
    }

    fn numeric(self) -> bool {
        !matches!(self, Self::Process | Self::User)
    }
}

/// Ascending order on `col`; ties fall back to the pid so equal rows never swap places between refreshes.
pub fn compare(a: &Process, b: &Process, col: Col) -> Ordering {
    let lower = |p: &Process| p.comm.chars().flat_map(char::to_lowercase).collect::<Vec<_>>();
    let primary = match col {
        Col::Process => lower(a).cmp(&lower(b)),
        Col::Memory => a.rss_bytes.cmp(&b.rss_bytes),
        Col::Cpu => a.cpu_pct.total_cmp(&b.cpu_pct),
        Col::Threads => a.threads.cmp(&b.threads),
        Col::Fds => a.fds.cmp(&b.fds),
        Col::User => a.user.cmp(&b.user),
        Col::Pid => Ordering::Equal,
    };
    primary.then(a.pid.cmp(&b.pid))
}

/// Case-insensitive substring of the name or command line, or the exact pid.
pub fn matches(p: &Process, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    p.comm.to_lowercase().contains(&query) || p.cmdline.to_lowercase().contains(&query) || p.pid.to_string() == query
}

/// Signals reach only our own processes; an unknown owner or an unknown current user allows none.
pub fn may_signal(p: &Process, me: Option<&str>) -> bool {
    me.is_some() && p.user.as_deref() == me
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line {
    /// Index into the table.
    pub row: usize,
    pub depth: u16,
    pub children: bool,
}

/// Tree by ppid over `order` (already sorted), flattened depth first; siblings keep `order`.
/// A row whose parent is absent is a root; rows caught in a parent cycle become roots in turn.
pub fn tree(table: &[Process], order: &[usize], collapsed: &HashSet<Key>) -> Vec<Line> {
    let present: HashSet<u32> = order.iter().map(|&i| table[i].pid).collect();
    let mut children: HashMap<u32, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for &i in order {
        let p = &table[i];
        if p.ppid != p.pid && present.contains(&p.ppid) {
            children.entry(p.ppid).or_default().push(i);
        } else {
            roots.push(i);
        }
    }
    let mut out = Vec::with_capacity(order.len());
    let mut seen = HashSet::new();
    let walk = |start: usize, out: &mut Vec<Line>, seen: &mut HashSet<usize>| {
        let mut stack = vec![(start, 0u16, true)];
        while let Some((row, depth, shown)) = stack.pop() {
            if !seen.insert(row) {
                continue;
            }
            let kids = children.get(&table[row].pid).map_or(&[][..], Vec::as_slice);
            if shown {
                out.push(Line {
                    row,
                    depth,
                    children: !kids.is_empty(),
                });
            }
            // Collapsed descendants are still walked, unshown, so they are not mistaken for cycle roots below.
            let open = shown && !collapsed.contains(&key(&table[row]));
            stack.extend(kids.iter().rev().map(|&k| (k, depth + 1, open)));
        }
    };
    for &root in &roots {
        walk(root, &mut out, &mut seen);
    }
    for &i in order {
        if !seen.contains(&i) {
            walk(i, &mut out, &mut seen);
        }
    }
    out
}

/// What the table shows: the latest scan, filtered, sorted, optionally as a tree, and the selection by key.
pub struct Model {
    pub table: Vec<Process>,
    pub sort: Col,
    pub descending: bool,
    pub filter: String,
    pub hierarchy: bool,
    pub collapsed: HashSet<Key>,
    pub selected: Option<Key>,
    pub lines: Vec<Line>,
}

impl Model {
    pub fn new(hierarchy: bool) -> Self {
        Self {
            table: Vec::new(),
            sort: Col::Memory,
            descending: true,
            filter: String::new(),
            hierarchy,
            collapsed: HashSet::new(),
            selected: None,
            lines: Vec::new(),
        }
    }

    pub fn set_table(&mut self, table: Vec<Process>) {
        self.table = table;
        self.rebuild();
    }

    pub fn rebuild(&mut self) {
        let table = &self.table;
        let mut order: Vec<usize> = (0..table.len()).filter(|&i| matches(&table[i], &self.filter)).collect();
        order.sort_by(|&a, &b| {
            let o = compare(&table[a], &table[b], self.sort);
            if self.descending { o.reverse() } else { o }
        });
        self.lines = if self.hierarchy {
            tree(table, &order, &self.collapsed)
        } else {
            order
                .into_iter()
                .map(|row| Line {
                    row,
                    depth: 0,
                    children: false,
                })
                .collect()
        };
    }

    pub fn process(&self, line: usize) -> Option<&Process> {
        self.lines.get(line).map(|l| &self.table[l.row])
    }

    pub fn selected_line(&self) -> Option<usize> {
        let selected = self.selected?;
        self.lines.iter().position(|l| key(&self.table[l.row]) == selected)
    }

    /// The selected process if it is still running, shown or filtered out.
    pub fn selected_process(&self) -> Option<&Process> {
        let selected = self.selected?;
        self.table.iter().find(|p| key(p) == selected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Signal {
    Term,
    Kill,
}

impl Signal {
    fn label(self) -> &'static str {
        match self {
            Self::Term => "End process",
            Self::Kill => "Force end",
        }
    }

    fn number(self) -> i32 {
        match self {
            Self::Term => libc::SIGTERM,
            Self::Kill => libc::SIGKILL,
        }
    }

    fn explain(self, p: &Process) -> String {
        match self {
            Self::Term => format!("PID {} receives SIGTERM and may save its work before it exits.", p.pid),
            Self::Kill => format!(
                "PID {} receives SIGKILL and stops at once; unsaved work is lost.",
                p.pid
            ),
        }
    }
}

fn send(p: &Process, signal: Signal) -> Result<(), String> {
    // A pid is reused once its process exits; refuse unless /proc still shows the same command.
    let comm = std::fs::read_to_string(format!("/proc/{}/comm", p.pid)).unwrap_or_default();
    if comm.trim_end() != p.comm {
        return Err("it has already exited".into());
    }
    // SAFETY: kill(2) takes plain integers and has no memory effects.
    if unsafe { libc::kill(p.pid as libc::pid_t, signal.number()) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn current_user() -> Option<String> {
    // SAFETY: getpwuid returns null or a pointer to static storage, read here before any other call can reuse it.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return None;
        }
        CStr::from_ptr((*pw).pw_name).to_str().ok().map(str::to_owned)
    }
}

fn confirm(p: Process, signal: Signal, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let p = p.clone();
        alert
            .title(SharedString::from(format!("{} “{}”?", signal.label(), p.comm)))
            .description(SharedString::from(signal.explain(&p)))
            .ok_text(signal.label())
            .ok_variant(ButtonVariant::Danger)
            .show_cancel(true)
            .on_ok(move |_, window, cx| {
                let note = match send(&p, signal) {
                    Ok(()) => Notification::success(format!("Sent {} to {} ({})", signal_name(signal), p.comm, p.pid)),
                    Err(e) => Notification::error(format!("Could not end {} ({}): {e}", p.comm, p.pid)),
                };
                window.push_notification(note, cx);
                true
            })
    });
}

fn signal_name(signal: Signal) -> &'static str {
    match signal {
        Signal::Term => "SIGTERM",
        Signal::Kill => "SIGKILL",
    }
}

/// An app icon from its `.desktop` file, else a generic glyph; kernel threads have no command line.
fn process_icon(p: &Process, icons: &IconIndex, size: f32, cx: &App) -> AnyElement {
    let glyph = if p.cmdline.is_empty() {
        IconName::Cpu
    } else {
        IconName::AppWindow
    };
    let color = theme(cx).text_tertiary;
    let generic = move || {
        Icon::new(glyph)
            .size(px(size))
            .text_color(color)
            .flex_shrink_0()
            .into_any_element()
    };
    match icons.lookup(&p.comm, &p.cmdline) {
        Some(path) => img(path.clone())
            .size(px(size))
            .flex_shrink_0()
            .with_fallback(generic)
            .into_any_element(),
        None => generic(),
    }
}

pub struct ProcessTable {
    pub model: Model,
    /// The Process column takes the width the others leave.
    process_width: f32,
    icons: Arc<IconIndex>,
    me: Option<String>,
}

impl TableDelegate for ProcessTable {
    fn columns_count(&self, _: &App) -> usize {
        Col::ALL.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.model.lines.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let col = Col::ALL[col_ix];
        let width = if col == Col::Process {
            self.process_width
        } else {
            col.width()
        };
        let column = Column::new(col.label(), col.label()).width(px(width)).sortable();
        let column = match (col == self.model.sort, self.model.descending) {
            (true, true) => column.descending(),
            (true, false) => column.ascending(),
            (false, _) => column,
        };
        if col.numeric() { column.text_right() } else { column }
    }

    fn perform_sort(&mut self, col_ix: usize, sort: ColumnSort, _: &mut Window, _: &mut Context<TableState<Self>>) {
        (self.model.sort, self.model.descending) = match sort {
            ColumnSort::Ascending => (Col::ALL[col_ix], false),
            ColumnSort::Descending => (Col::ALL[col_ix], true),
            ColumnSort::Default => (Col::Memory, true),
        };
        self.model.rebuild();
    }

    fn render_th(&mut self, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let col = Col::ALL[col_ix];
        div()
            .size_full()
            .flex()
            .items_center()
            .when(col.numeric(), |d| d.justify_end())
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme(cx).text)
            .child(col.label())
    }

    fn render_tr(&mut self, row_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let row_key = self.model.process(row_ix).map(key);
        let selected = row_key.is_some() && row_key == self.model.selected;
        let tint = theme(cx).selection;
        div()
            .id(("row", row_ix))
            .when(selected, |d| d.bg(tint))
            .on_click(cx.listener(move |table, _, _, cx| {
                table.delegate_mut().model.selected = row_key;
                cx.notify();
            }))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let (Some(line), Some(p)) = (self.model.lines.get(row_ix).copied(), self.model.process(row_ix)) else {
            return div().into_any_element();
        };
        let t = theme(cx);
        let col = Col::ALL[col_ix];
        let number = |text: String| {
            div()
                .w_full()
                .flex()
                .justify_end()
                .font_features(t.numbers.clone())
                .child(text)
                .into_any_element()
        };
        let unreadable = |id: &'static str| {
            div()
                .id((id, row_ix))
                .w_full()
                .flex()
                .justify_end()
                .text_color(t.text_tertiary)
                .child("–")
                .tooltip(|window, cx| Tooltip::new("not permitted").build(window, cx))
                .into_any_element()
        };
        match col {
            Col::Process => {
                let row_key = key(p);
                let open = !self.model.collapsed.contains(&row_key);
                let disclosure = if line.children {
                    div()
                        .id(("disclose", row_ix))
                        .flex_shrink_0()
                        .child(
                            Icon::new(if open {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size(px(12.))
                            .text_color(t.text_secondary),
                        )
                        .on_click(cx.listener(move |table, _, _, cx| {
                            cx.stop_propagation();
                            let model = &mut table.delegate_mut().model;
                            if !model.collapsed.remove(&row_key) {
                                model.collapsed.insert(row_key);
                            }
                            model.rebuild();
                            cx.notify();
                        }))
                        .into_any_element()
                } else {
                    div().w(px(12.)).flex_shrink_0().into_any_element()
                };
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .min_w_0()
                    .pl(px(f32::from(line.depth) * INDENT))
                    .when(self.model.hierarchy, |d| d.child(disclosure))
                    .child(process_icon(p, &self.icons, 16., cx))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(p.comm.clone()),
                    )
                    .into_any_element()
            }
            Col::Memory => number(bytes(p.rss_bytes as f64)),
            Col::Cpu => number(format!("{:.1}%", p.cpu_pct)),
            Col::Threads => number(p.threads.to_string()),
            Col::Fds => p.fds.map_or_else(|| unreadable("fds"), |n| number(n.to_string())),
            Col::User => match &p.user {
                Some(user) => div()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(t.text_secondary)
                    .child(user.clone())
                    .into_any_element(),
                None => div().text_color(t.text_tertiary).child("–").into_any_element(),
            },
            Col::Pid => number(p.pid.to_string()),
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(p) = self.model.process(row_ix).cloned() else {
            return menu;
        };
        let allowed = may_signal(&p, self.me.as_deref());
        let copy = |label: &'static str, text: String| {
            PopupMenuItem::new(label)
                .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())))
        };
        let end = |signal: Signal, p: Process| {
            PopupMenuItem::new(signal.label())
                .disabled(!allowed)
                .on_click(move |_, window, cx| confirm(p.clone(), signal, window, cx))
        };
        menu.item(copy("Copy name", p.comm.clone()))
            .item(copy("Copy PID", p.pid.to_string()))
            .item(copy("Copy command line", p.cmdline.clone()))
            .separator()
            .item(end(Signal::Term, p.clone()))
            .item(end(Signal::Kill, p))
    }

    fn render_empty(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let t = theme(cx);
        div()
            .size_full()
            .flex()
            .justify_center()
            .pt(px(40.))
            .text_size(t.caption)
            .text_color(t.text_secondary)
            .child("No matching processes")
    }
}

/// Hierarchy mode, kept for the session because tabs are unmounted when hidden.
#[derive(Default)]
struct HierarchyMode(bool);

impl Global for HierarchyMode {}

/// Built once per run on the background executor.
struct Icons(Arc<IconIndex>);

impl Global for Icons {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Card {
    Cpu,
    Load,
    Temp,
}

impl Card {
    const ALL: [Card; 3] = [Self::Cpu, Self::Load, Self::Temp];

    fn source(self) -> Source {
        let path = match self {
            Self::Cpu => "/cpu/total_pct",
            Self::Load => "/system/load/0",
            Self::Temp => "/cpu/temp_c",
        };
        Source::new(vec![path.into()], Combine::Sum)
    }
}

/// History read for one range and one selected process.
struct Stored {
    range: Range,
    key: Option<Key>,
    /// `None` when the daemon has not recorded the process yet.
    proc_id: Option<i64>,
    cards: HashMap<Card, Vec<Bucket>>,
    fields: HashMap<ProcField, Vec<Bucket>>,
}

fn load(range: Range, to: i64, selected: Option<Key>) -> Result<Stored> {
    let history = History::open(&db_path())?;
    let from = to - range.span_s();
    let mut cards = HashMap::new();
    for card in Card::ALL {
        cards.insert(card, card.source().query(&history, from, to, BUCKETS)?);
    }
    let proc_id = match selected {
        Some((pid, start)) => history.proc_id(pid, start)?,
        None => None,
    };
    let mut fields = HashMap::new();
    if let Some(id) = proc_id {
        for field in FIELDS {
            fields.insert(field, history.proc_series(id, field, from, to, BUCKETS)?);
        }
    }
    Ok(Stored {
        range,
        key: selected,
        proc_id,
        cards,
        fields,
    })
}

fn field_value(p: &Process, field: ProcField) -> Option<f64> {
    match field {
        ProcField::Cpu => Some(p.cpu_pct),
        ProcField::Rss => Some(p.rss_bytes as f64),
        ProcField::Threads => Some(p.threads as f64),
        ProcField::Fds => p.fds.map(|n| n as f64),
        ProcField::Read => p.read_bps.map(|n| n as f64),
        ProcField::Write => p.write_bps.map(|n| n as f64),
    }
}

fn wall_clock() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0., |d| d.as_secs_f64())
}

pub struct Processes {
    live: Entity<Live>,
    feed: Entity<ProcessFeed>,
    table: Option<Entity<TableState<ProcessTable>>>,
    /// The delegate until the first render, which has the window the table and search field need.
    pending: Option<ProcessTable>,
    search: Option<Entity<InputState>>,
    range: Range,
    stored: Option<Stored>,
    loading: bool,
    attempt: Option<(Range, Option<Key>, f64)>,
    error: Option<SharedString>,
    /// The selected process as last scanned; kept after it exits.
    inspected: Option<Process>,
    /// Live scans of the selected process since it was selected, to reach past the last recorded point.
    trail: Vec<(f64, Process)>,
    cards: HashMap<Card, Rc<[Bucket]>>,
    fields: HashMap<ProcField, Rc<[Bucket]>>,
    span: (f64, f64),
    width_s: i64,
    version: u64,
    charts: [Rc<RefCell<ChartState>>; 4],
    cpu_meta: Option<CpuMeta>,
    #[cfg(debug_assertions)]
    dev_select: Option<String>,
}

impl Processes {
    pub fn new(live: Entity<Live>, feed: Entity<ProcessFeed>, cx: &mut Context<Self>) -> Self {
        cx.observe(&live, |this, _, cx| this.refresh(cx)).detach();
        cx.observe(&feed, |this, _, cx| this.on_feed(cx)).detach();
        let icons = match cx.try_global::<Icons>() {
            Some(icons) => icons.0.clone(),
            None => {
                let task = cx.background_executor().spawn(async { IconIndex::load() });
                cx.spawn(async move |this, cx| {
                    let icons = Arc::new(task.await);
                    cx.update(|cx| cx.set_global(Icons(icons.clone())));
                    this.update(cx, |this, cx| this.with_table(cx, |table| table.icons = icons))
                        .ok();
                })
                .detach();
                Arc::default()
            }
        };
        let cursor = Rc::new(Cell::new(None));
        let mut this = Self {
            live,
            feed,
            table: None,
            pending: Some(ProcessTable {
                model: Model::new(cx.try_global::<HierarchyMode>().is_some_and(|h| h.0)),
                icons,
                me: current_user(),
                process_width: Col::Process.width(),
            }),
            search: None,
            range: remembered(Tab::Processes, cx),
            stored: None,
            loading: false,
            attempt: None,
            error: None,
            inspected: None,
            trail: Vec::new(),
            cards: HashMap::new(),
            fields: HashMap::new(),
            span: (0., 0.),
            width_s: 1,
            version: 0,
            charts: std::array::from_fn(|_| ChartState::with_cursor(cursor.clone())),
            cpu_meta: None,
            #[cfg(debug_assertions)]
            dev_select: std::env::var("SYSMAN_DEV_SELECT").ok(),
        };
        #[cfg(debug_assertions)]
        if std::env::var_os("SYSMAN_DEV_HIERARCHY").is_some() {
            this.with_table(cx, |table| table.model.hierarchy = true);
        }
        this.on_feed(cx);
        this
    }

    /// The table, the cards' history and, once a process is selected, its history are all in.
    #[cfg(debug_assertions)]
    pub fn ready(&self, cx: &App) -> bool {
        let selected = self.delegate(cx).model.selected;
        self.feed.read(cx).latest.is_some()
            && self.live.read(cx).latest.is_some()
            && self.dev_select.is_none()
            && !self.loading
            && self
                .attempt
                .is_some_and(|(range, key, _)| range == self.range && key == selected)
    }

    fn delegate<'a>(&'a self, cx: &'a App) -> &'a ProcessTable {
        match &self.table {
            Some(table) => table.read(cx).delegate(),
            None => self
                .pending
                .as_ref()
                .expect("the delegate is pending until the table exists"),
        }
    }

    fn with_table<R>(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut ProcessTable) -> R) -> R {
        match &self.table {
            Some(table) => table.update(cx, |state, cx| {
                let r = f(state.delegate_mut());
                cx.notify();
                r
            }),
            None => f(self
                .pending
                .as_mut()
                .expect("the delegate is pending until the table exists")),
        }
    }

    fn now(&self, cx: &App) -> f64 {
        self.live.read(cx).latest.as_ref().map_or_else(wall_clock, |s| s.t)
    }

    fn on_feed(&mut self, cx: &mut Context<Self>) {
        let Some((t, table)) = self.feed.read(cx).latest.clone() else {
            return;
        };
        self.with_table(cx, |delegate| delegate.model.set_table(table.rows));
        #[cfg(debug_assertions)]
        if let Some(name) = self.dev_select.take() {
            let line = self.with_table(cx, |d| {
                let line = d.model.lines.iter().position(|l| d.model.table[l.row].comm == name)?;
                d.model.selected = d.model.process(line).map(key);
                Some(line)
            });
            match line {
                Some(line) => {
                    if let Some(table) = &self.table {
                        table.update(cx, |state, cx| state.scroll_to_row(line, cx));
                    }
                }
                None => self.dev_select = Some(name),
            }
        }
        self.sync_selection(cx);
        if let Some(p) = self.delegate(cx).model.selected_process().cloned() {
            self.trail.retain(|(at, _)| *at >= t - TRAIL_S);
            self.trail.push((t, p.clone()));
            self.inspected = Some(p);
        }
        self.refresh(cx);
    }

    /// Follows a change of the selected row: a new process starts a new trail and history read.
    fn sync_selection(&mut self, cx: &mut Context<Self>) {
        let selected = self.delegate(cx).model.selected;
        if selected == self.inspected.as_ref().map(key) {
            return;
        }
        self.inspected = self.delegate(cx).model.selected_process().cloned();
        self.trail.clear();
        if let (Some(p), Some((t, _))) = (&self.inspected, &self.feed.read(cx).latest) {
            self.trail.push((*t, p.clone()));
        }
        self.refresh(cx);
    }

    fn set_range(&mut self, range: Range, cx: &mut Context<Self>) {
        if range != self.range {
            self.range = range;
            remember(Tab::Processes, range, cx);
            self.refresh(cx);
        }
    }

    fn toggle_hierarchy(&mut self, cx: &mut Context<Self>) {
        let on = self.with_table(cx, |table| {
            table.model.hierarchy = !table.model.hierarchy;
            table.model.rebuild();
            table.model.hierarchy
        });
        cx.set_global(HierarchyMode(on));
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let now = self.now(cx);
        let selected = self.inspected.as_ref().map(key);
        let stale = self
            .attempt
            .is_none_or(|(range, key, at)| range != self.range || key != selected || now - at > RELOAD_S);
        if stale && !self.loading {
            self.load(now, selected, cx);
        }
        self.merge(now, cx);
        cx.notify();
    }

    fn load(&mut self, now: f64, selected: Option<Key>, cx: &mut Context<Self>) {
        self.loading = true;
        self.attempt = Some((self.range, selected, now));
        let range = self.range;
        let task = cx
            .background_executor()
            .spawn(async move { load(range, now as i64, selected) });
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
        for card in Card::ALL {
            let history = stored.and_then(|s| s.cards.get(&card)).map_or(&[][..], Vec::as_slice);
            let merged = card.source().top_up(history, live, from, self.width_s);
            self.cards.insert(card, merged.into());
        }
        let selected = self.inspected.as_ref().map(key);
        let stored = stored.filter(|s| s.key == selected);
        let start = self.trail.first().map_or(f64::INFINITY, |(t, _)| *t);
        for field in FIELDS {
            let history = stored.and_then(|s| s.fields.get(&field)).map_or(&[][..], Vec::as_slice);
            let points = self
                .trail
                .iter()
                .filter_map(|(t, p)| Some(((t * 1000.).round() as i64, field_value(p, field)?)));
            let merged = top_up(history, start, points, from, self.width_s);
            self.fields.insert(field, merged.into());
        }
        self.span = (from as f64, now);
        self.version += 1;
    }

    fn data<K: std::hash::Hash + Eq>(map: &HashMap<K, Rc<[Bucket]>>, k: K) -> Rc<[Bucket]> {
        map.get(&k).cloned().unwrap_or_else(|| Rc::from(Vec::new()))
    }

    fn peak(&self, card: Card) -> Option<f64> {
        let (from, to) = self.span;
        Self::data(&self.cards, card)
            .iter()
            .filter(|b| b.t as f64 + self.width_s as f64 >= from && b.t as f64 <= to)
            .map(|b| b.max)
            .reduce(f64::max)
    }

    fn spark(&self, card: Card, color: Hsla) -> Sparkline {
        let gap = (self.width_s as f64 * 1.5).max(5.);
        Sparkline::new(
            Self::data(&self.cards, card),
            color,
            self.span,
            self.width_s as f64,
            gap,
        )
    }
}

fn caption(text: impl Into<SharedString>, cx: &App) -> Div {
    let t = theme(cx);
    div()
        .text_size(t.small_caption)
        .text_color(t.text_secondary)
        .child(text.into())
}

fn value_row(value: String, detail: Option<String>, cx: &App) -> impl IntoElement {
    let t = theme(cx);
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .child(
            div()
                .text_size(t.card_value)
                .font_weight(FontWeight::SEMIBOLD)
                .font_features(t.numbers.clone())
                .child(value),
        )
        .child(div().flex_1())
        .children(detail.map(|d| caption(d, cx).font_features(t.numbers.clone())))
}

impl Processes {
    fn header(&self, sample: Option<&Sample>, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let cpu = sample.and_then(|s| s.cpu.as_ref());
        let load = sample.and_then(|s| s.system.as_ref()).map(|s| s.load);
        let cores = cpu.and_then(|c| c.core_pct.clone()).unwrap_or_default();
        let groups = Groups::new(self.cpu_meta.as_ref(), t);
        let bars = cores
            .iter()
            .enumerate()
            .map(|(i, &pct)| {
                let (group, color) = groups
                    .of(i as u32)
                    .map_or(("CPU".into(), t.blue), |g| (g.0.clone(), g.1));
                CoreBar { pct, color, group }
            })
            .collect();
        let core_legend = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(10.))
            .children(groups.groups.iter().map(|(label, color, cpus)| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(5.))
                    .text_size(t.small_caption)
                    .text_color(t.text_secondary)
                    .font_features(t.numbers.clone())
                    .child(div().size(px(8.)).rounded(px(2.)).bg(*color))
                    .child(format!("{label} · {} · {:.0}%", cpus.len(), Groups::mean(&cores, cpus)))
            }));
        let topology = self.cpu_meta.as_ref().map(|m| &m.topology);
        let cores_label = topology
            .and_then(|t| t.cores)
            .map(|c| format!("{c} cores"))
            .unwrap_or_default();
        let total = cpu.map(|c| c.total_pct);
        let split = cpu.map(|c| {
            format!(
                "{:.0}% user · {:.0}% sys",
                c.breakdown_pct.user + c.breakdown_pct.nice,
                c.breakdown_pct.system + c.breakdown_pct.irq + c.breakdown_pct.softirq
            )
        });
        let temp = cpu.and_then(|c| c.temp_c);
        let peak = |card: Card, unit: Unit| self.peak(card).map(|v| format!("peak {}", unit.format(v)));
        let with_peak = |spark: Sparkline, label: Option<String>| match label {
            Some(label) => spark.caption(label),
            None => spark,
        };
        let count = self.feed.read(cx).table().map(|t| t.count);
        let cpu_color = cpu_colour(total.unwrap_or(0.), t);
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .px(t.page_padding)
            .pt(px(14.))
            .pb(px(10.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .child(caption_label("Processor", cx))
                    .child(div().flex_1())
                    .child(caption(cores_label, cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(10.))
                    .child(
                        card("card-cpu", "CPU usage", cpu_color, cx)
                            .child(value_row(total.map_or("–".into(), |v| format!("{v:.0}%")), split, cx))
                            .child(with_peak(
                                self.spark(Card::Cpu, cpu_color).fixed(100., "100%"),
                                peak(Card::Cpu, Unit::Percent),
                            )),
                    )
                    .child(
                        card("card-cores", "Cores", t.blue, cx)
                            .child(core_bars(bars, px(58.), cx))
                            .child(core_legend),
                    )
                    .child(
                        card("card-load", "Load average", t.orange, cx)
                            .child(value_row(
                                load.map_or("–".into(), |l| format!("{:.2}", l[0])),
                                load.map(|l| format!("5 min {:.2} · 15 min {:.2}", l[1], l[2])),
                                cx,
                            ))
                            .child(with_peak(
                                self.spark(Card::Load, t.orange),
                                self.peak(Card::Load).map(|v| format!("peak {v:.2}")),
                            )),
                    )
                    .child(
                        card("card-temp", "CPU temperature", t.orange, cx)
                            .child(value_row(
                                temp.map_or("–".into(), |v| Unit::Celsius.format(v)),
                                None,
                                cx,
                            ))
                            .child(with_peak(
                                self.spark(Card::Temp, t.orange),
                                peak(Card::Temp, Unit::Celsius),
                            )),
                    ),
            )
            .child(
                caption(count.map_or(String::new(), |c| format!("{c} processes")), cx)
                    .font_features(t.numbers.clone())
                    .when_some(self.error.clone(), |d, e| d.child(div().text_color(t.orange).child(e))),
            )
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let hierarchy = self.delegate(cx).model.hierarchy;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .py(px(4.))
            .border_t_1()
            .border_b_1()
            .border_color(t.hairline)
            .child(div().flex_1().children(self.search.as_ref().map(|search| {
                Input::new(search)
                    .appearance(false)
                    .cleanable(true)
                    .prefix(Icon::new(IconName::Search).small().text_color(t.text_secondary))
            })))
            .child(
                Button::new("hierarchy")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::ListTree))
                    .label("Hierarchy")
                    .selected(hierarchy)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_hierarchy(cx))),
            )
    }

    #[allow(clippy::too_many_arguments)]
    fn chart_section(
        &self,
        icon: IconName,
        title: &str,
        chart: TrendChart,
        legend: Option<AnyElement>,
        note: &str,
        cx: &App,
    ) -> impl IntoElement {
        let t = theme(cx);
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(7.))
                    .child(Icon::new(icon).small().text_color(t.text_secondary))
                    .child(
                        div()
                            .text_size(t.panel_title)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title.to_owned()),
                    )
                    .child(div().flex_1())
                    .children(legend),
            )
            .child(chart)
            .child(caption(note.to_owned(), cx))
    }

    fn chart(&self, i: usize) -> TrendChart {
        let gap = (self.width_s as f64 * 1.5).max(RECORD_INTERVAL_S * 2.5);
        TrendChart::new(
            self.charts[i].clone(),
            self.version,
            self.span,
            self.width_s as f64,
            gap,
        )
        .relative_axis()
        .y(YScale::Zero)
        .height(px(CHART_HEIGHT))
    }

    fn inspector(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let cx: &App = cx;
        let t = theme(cx);
        let body = div().flex().flex_col().gap(px(16.)).p(px(16.));
        let Some(p) = &self.inspected else {
            return body
                .child(caption("Select a process to see its details and history.", cx))
                .into_any_element();
        };
        let icons = self.delegate(cx).icons.clone();
        let running = self.delegate(cx).model.selected_process().is_some();
        let stored = self
            .stored
            .as_ref()
            .filter(|s| s.range == self.range && s.key == Some(key(p)));
        let status = if !running {
            Some("This process has exited; its last values are shown.")
        } else if stored.is_some_and(|s| s.proc_id.is_none()) {
            Some("No recorded history yet (it started less than 10 s ago); showing live samples only.")
        } else {
            None
        };
        let kv = |label: &str, value: String| {
            div()
                .flex()
                .flex_row()
                .gap(px(8.))
                .text_size(t.caption)
                .child(div().w(px(64.)).text_color(t.text_secondary).child(label.to_owned()))
                .child(div().flex_1().min_w_0().font_features(t.numbers.clone()).child(value))
        };
        let details = hairline(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .py(px(10.))
                .px(px(12.))
                .rounded(t.card_radius)
                .bg(t.card)
                .border_color(t.hairline),
            t.hairline_width,
        )
        .child(kv("User", p.user.clone().unwrap_or_else(|| "–".into())))
        .child(kv("Started", clock(p.start_time as f64, "%b %-d %H:%M:%S")))
        .child(kv("State", p.state.clone()))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .child(caption("Command line", cx))
                .child(
                    div()
                        .text_size(t.caption)
                        .font_family(t.mono.clone())
                        .text_color(t.text)
                        .child(if p.cmdline.is_empty() {
                            format!("[{}]", p.comm)
                        } else {
                            p.cmdline.clone()
                        }),
                ),
        );
        let swatch = |color: Hsla, label: &'static str| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .text_size(t.small_caption)
                .text_color(t.text_secondary)
                .child(div().size(px(6.)).rounded_full().bg(color))
                .child(label)
        };
        let field = |f: ProcField| Self::data(&self.fields, f);
        let fds_note = if p.fds.is_some() {
            "Open files, sockets and pipes. A steady climb can signal a handle leak."
        } else {
            "Not permitted: the collector cannot read another user's descriptors."
        };
        let io_note = if p.read_bps.is_some() {
            "Bytes read from and written to storage, per second."
        } else {
            "Not permitted: the collector cannot read another user's I/O."
        };
        body.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .child(process_icon(p, &icons, 32., cx))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(p.comm.clone()),
                        )
                        .child(caption(format!("PID {}", p.pid), cx).font_features(t.numbers.clone())),
                ),
        )
        .child(details)
        .when_some(status, |d, s| d.child(caption(s, cx).text_color(t.orange)))
        .child(range_picker(
            self.range,
            move |range, _, cx| {
                this.update(cx, |p, cx| p.set_range(range, cx)).ok();
            },
            cx,
        ))
        .child(
            self.chart_section(
                IconName::MemoryStick,
                "Memory (RSS)",
                self.chart(0)
                    .line(field(ProcField::Rss), t.blue, "RSS")
                    .unit(Unit::Bytes),
                None,
                "Resident set size: pages in RAM, shared pages counted in full.",
                cx,
            ),
        )
        .child(
            self.chart_section(
                IconName::Cpu,
                "CPU",
                self.chart(1)
                    .line(field(ProcField::Cpu), t.green, "CPU")
                    .unit(Unit::Percent),
                None,
                "Percent of one core, from the CPU-time delta between scans; can exceed 100%.",
                cx,
            ),
        )
        .child(
            self.chart_section(
                IconName::FileText,
                "File descriptors",
                self.chart(2)
                    .line(field(ProcField::Fds), t.purple, "FDs")
                    .unit(Unit::Index),
                None,
                fds_note,
                cx,
            ),
        )
        .child(
            self.chart_section(
                IconName::HardDrive,
                "Disk I/O",
                self.chart(3)
                    .line(field(ProcField::Read), t.blue, "Read")
                    .line(field(ProcField::Write), t.orange, "Write")
                    .unit(Unit::BytesPerSec),
                Some(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(10.))
                        .child(swatch(t.blue, "Read"))
                        .child(swatch(t.orange, "Write"))
                        .into_any_element(),
                ),
                io_note,
                cx,
            ),
        )
        .into_any_element()
    }

    /// Builds what needs a window: the table and the search field.
    fn ensure_widgets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_none() {
            let search = cx.new(|cx| InputState::new(window, cx).placeholder("Filter by name"));
            cx.subscribe(&search, |this, search, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = search.read(cx).value().to_string();
                    this.with_table(cx, |table| {
                        table.model.filter = query;
                        table.model.rebuild();
                    });
                }
            })
            .detach();
            self.search = Some(search);
        }
        if let Some(delegate) = self.pending.take() {
            let line = delegate.model.selected_line();
            let table = cx.new(|cx| {
                let mut state = TableState::new(delegate, window, cx)
                    .row_selectable(false)
                    .col_movable(false);
                if let Some(line) = line {
                    state.scroll_to_row(line, cx);
                }
                state
            });
            cx.observe(&table, |this, _, cx| this.sync_selection(cx)).detach();
            self.table = Some(table);
        }
    }
}

impl Render for Processes {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_widgets(window, cx);
        let others: f32 = Col::ALL[1..].iter().map(|c| c.width()).sum();
        let fill = (f32::from(window.viewport_size().width) - INSPECTOR_WIDTH - others - 16.)
            .max(Col::Process.width())
            .floor();
        if let Some(table) = &self.table
            && table.read(cx).delegate().process_width != fill
        {
            table.update(cx, |state, cx| {
                state.delegate_mut().process_width = fill;
                state.refresh(cx);
            });
        }
        let sample = self.live.read(cx).latest.clone();
        let toolbar = self.toolbar(cx).into_any_element();
        let inspector = self.inspector(cx);
        let t = theme(cx);
        let table = self.table.clone().expect("built above");
        div()
            .size_full()
            .flex()
            .flex_row()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(self.header(sample.as_ref(), cx))
                    .child(toolbar)
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .text_size(px(13.))
                            .font_features(t.numbers.clone())
                            .child(
                                DataTable::new(&table)
                                    .stripe(true)
                                    .bordered(false)
                                    .with_size(Size::XSmall),
                            ),
                    ),
            )
            .child(
                div()
                    .id("process-inspector")
                    .flex_shrink_0()
                    .w(px(INSPECTOR_WIDTH))
                    .h_full()
                    .overflow_y_scroll()
                    .border_l_1()
                    .border_color(t.hairline)
                    .bg(t.panel)
                    .child(inspector),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: u32, comm: &str, rss: u64) -> Process {
        Process {
            pid,
            ppid,
            comm: comm.into(),
            state: "S".into(),
            threads: 1,
            rss_bytes: rss,
            cpu_pct: 0.,
            read_bps: None,
            write_bps: None,
            user: Some("me".into()),
            fds: None,
            net_rx_bps: None,
            net_tx_bps: None,
            cmdline: format!("/usr/bin/{comm} --flag"),
            start_time: 1000 + u64::from(pid),
        }
    }

    fn shown(model: &Model) -> Vec<(u32, u16)> {
        model.lines.iter().map(|l| (model.table[l.row].pid, l.depth)).collect()
    }

    #[test]
    fn the_tree_nests_children_under_parents_in_sort_order() {
        let mut model = Model::new(true);
        model.set_table(vec![
            proc(1, 0, "init", 10),
            proc(20, 1, "small", 20),
            proc(30, 1, "big", 300),
            proc(31, 30, "child", 5),
            proc(40, 999, "orphan", 50),
        ]);
        assert_eq!(
            shown(&model),
            [(40, 0), (1, 0), (30, 1), (31, 2), (20, 1)],
            "memory descending within each parent; a row whose parent is gone is a root"
        );
        assert!(model.lines[1].children && !model.lines[4].children);

        model.collapsed.insert(key(&model.table[2]));
        model.rebuild();
        assert_eq!(
            shown(&model),
            [(40, 0), (1, 0), (30, 1), (20, 1)],
            "collapsing hides the subtree"
        );
    }

    #[test]
    fn a_parent_cycle_neither_hangs_nor_loses_rows() {
        let table = vec![proc(5, 6, "a", 1), proc(6, 5, "b", 2), proc(7, 7, "self", 3)];
        let order = [2, 1, 0];
        let lines = tree(&table, &order, &HashSet::new());
        let mut rows: Vec<usize> = lines.iter().map(|l| l.row).collect();
        rows.sort();
        assert_eq!(rows, [0, 1, 2], "every row appears exactly once");
        let collapsed: HashSet<Key> = [key(&table[1])].into();
        let lines = tree(&table, &order, &collapsed);
        assert_eq!(
            lines.iter().map(|l| l.row).collect::<Vec<_>>(),
            [2, 1],
            "a collapsed cycle member hides its partner instead of promoting it to a root"
        );
    }

    #[test]
    fn selection_follows_the_process_through_resorts_and_refreshes() {
        let mut model = Model::new(false);
        model.set_table(vec![proc(1, 0, "a", 100), proc(2, 0, "b", 300), proc(3, 0, "c", 200)]);
        model.selected = Some(key(&model.table[2]));
        assert_eq!(model.selected_line(), Some(1));

        model.sort = Col::Pid;
        model.descending = false;
        model.rebuild();
        assert_eq!(
            model.selected_line(),
            Some(2),
            "re-sorting moves the row, not the selection"
        );

        let mut next = vec![proc(4, 0, "d", 1), proc(3, 0, "c", 999), proc(1, 0, "a", 100)];
        next[1].cpu_pct = 50.;
        model.set_table(next.clone());
        assert_eq!(model.process(model.selected_line().unwrap()).unwrap().pid, 3);

        // The same pid reused by a new process is a different instance.
        next[1].start_time += 1;
        model.set_table(next);
        assert_eq!(model.selected_line(), None);
        assert_eq!(
            model.selected,
            Some((3, 1003)),
            "the key survives for when it reappears"
        );
    }

    #[test]
    fn equal_values_keep_a_stable_order() {
        let mut model = Model::new(false);
        model.set_table(vec![proc(9, 0, "x", 5), proc(3, 0, "y", 5), proc(5, 0, "z", 5)]);
        let first = shown(&model);
        model.set_table(vec![proc(5, 0, "z", 5), proc(9, 0, "x", 5), proc(3, 0, "y", 5)]);
        assert_eq!(shown(&model), first, "input order does not leak into ties");
        let mut a = proc(1, 0, "a", 1);
        let b = proc(2, 0, "b", 1);
        a.fds = Some(3);
        assert_eq!(
            compare(&a, &b, Col::Fds),
            Ordering::Greater,
            "an unreadable count sorts below any value"
        );
        assert_eq!(
            compare(&proc(1, 0, "Zed", 0), &proc(2, 0, "alpha", 0), Col::Process),
            Ordering::Greater
        );
    }

    #[test]
    fn the_filter_matches_name_command_line_or_exact_pid() {
        let p = proc(4242, 1, "firefox", 1);
        assert!(matches(&p, "FIRE"));
        assert!(matches(&p, "usr/bin"), "command line");
        assert!(matches(&p, " 4242 "));
        assert!(!matches(&p, "424"), "a partial pid would match too much");
        assert!(matches(&p, ""));
        assert!(!matches(&p, "chrome"));

        let mut model = Model::new(true);
        model.set_table(vec![proc(1, 0, "init", 1), proc(2, 1, "firefox", 1)]);
        model.filter = "fire".into();
        model.rebuild();
        assert_eq!(
            shown(&model),
            [(2, 0)],
            "a match whose parent is filtered out becomes a root"
        );
    }

    #[test]
    fn only_our_own_processes_can_be_signalled() {
        let mut p = proc(1, 0, "x", 1);
        assert!(may_signal(&p, Some("me")));
        assert!(!may_signal(&p, Some("root")));
        assert!(!may_signal(&p, None), "an unknown current user may signal nothing");
        p.user = None;
        assert!(!may_signal(&p, Some("me")), "an unknown owner is not ours");
    }
}
