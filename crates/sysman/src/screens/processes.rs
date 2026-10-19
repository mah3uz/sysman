use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::rc::Rc;
use std::sync::{Arc, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use gpui::{
    Anchor, AnyElement, App, AppContext, ClipboardItem, Context, Div, Entity, FocusHandle, Global, Hsla,
    InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement, Render, SharedString, Stateful,
    StatefulInteractiveElement, Styled, WeakEntity, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_component::tooltip::Tooltip;
use gpui_component::{Icon, Sizable, Size, WindowExt};
use gpui_kit_assets::IconName;
use schema::Sample;
use schema::cpu::CpuMeta;
use schema::processes::Process;
use serde::{Deserialize, Serialize};

use crate::charts::core_bars::{CoreBar, core_bars};
use crate::charts::format::{Unit, bytes};
use crate::charts::sparkline::Sparkline;
use crate::charts::trend::{ChartState, TrendChart, YScale, clock};
use crate::data::desktop::IconIndex;
use crate::data::helper;
use crate::data::history::{Bucket, History, ProcField, bucket_width, db_path};
use crate::data::live::Live;
use crate::data::prefs::{Prefs, prefs};
use crate::data::process::ProcessFeed;
use crate::data::source::{Combine, Source, top_up};
use crate::screens::dashboard::{Groups, cpu_colour};
use crate::ui::shell::{CONTROL_HEIGHT, Focus, Tab, pill, take_focus};
use crate::ui::theme::{rem_px, theme};
use crate::ui::widgets::app_icon::app_icon;
use crate::ui::widgets::card::card;
use crate::ui::widgets::detail_sheet::{Detail, Series, opener};
use crate::ui::widgets::hairline;
use crate::ui::widgets::hint::helper_hint;
use crate::ui::widgets::panel::caption_label;
use crate::ui::widgets::range::{Range, range_picker, remember_in, remembered, remembered_in};
use crate::ui::widgets::sidebar::{self, Side, Sidebar, Toggled, Widths};
use crate::ui::widgets::text::caption;

const BUCKETS: u32 = 120;
const RELOAD_S: f64 = 60.;
const INSPECTOR_RANGE: &str = "inspector";
const INSPECTOR: Widths = Widths::new(420., 320., 640.);
const COLUMNS_KEY: &str = "processes.columns";
const SORT_KEY: &str = "processes.sort";
const INSPECTOR_KEY: &str = "processes.inspector";
/// Room the table leaves for its vertical scrollbar.
const SCROLLBAR: f32 = 16.;
const CELL_PAD: f32 = 6.;
const TABLE_TEXT: f32 = 14.;
const TOOLBAR_HEIGHT: f32 = 46.;
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
    Command,
    Path,
    Memory,
    MemPct,
    Virtual,
    Cpu,
    CpuTime,
    Threads,
    Fds,
    Read,
    Write,
    NetRx,
    NetTx,
    User,
    State,
    Nice,
    Priority,
    Started,
    Pid,
    Ppid,
}

impl Col {
    /// Every column, in display order.
    pub const ALL: [Col; 21] = [
        Self::Process,
        Self::Command,
        Self::Path,
        Self::Memory,
        Self::MemPct,
        Self::Virtual,
        Self::Cpu,
        Self::CpuTime,
        Self::Threads,
        Self::Fds,
        Self::Read,
        Self::Write,
        Self::NetRx,
        Self::NetTx,
        Self::User,
        Self::State,
        Self::Nice,
        Self::Priority,
        Self::Started,
        Self::Pid,
        Self::Ppid,
    ];
    const DEFAULT: [Col; 7] = [
        Self::Process,
        Self::Memory,
        Self::Cpu,
        Self::Threads,
        Self::Fds,
        Self::User,
        Self::Pid,
    ];

    /// The default columns, with the Net pair when the helper reports per-process traffic.
    pub fn defaults(net: bool) -> Vec<Col> {
        Self::ALL
            .into_iter()
            .filter(|c| Self::DEFAULT.contains(c) || (net && matches!(c, Self::NetRx | Self::NetTx)))
            .collect()
    }

    fn key(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Command => "command",
            Self::Path => "path",
            Self::Memory => "memory",
            Self::MemPct => "mem_pct",
            Self::Virtual => "virtual",
            Self::Cpu => "cpu",
            Self::CpuTime => "cpu_time",
            Self::Threads => "threads",
            Self::Fds => "fds",
            Self::Read => "read",
            Self::Write => "write",
            Self::NetRx => "net_rx",
            Self::NetTx => "net_tx",
            Self::User => "user",
            Self::State => "state",
            Self::Nice => "nice",
            Self::Priority => "priority",
            Self::Started => "started",
            Self::Pid => "pid",
            Self::Ppid => "ppid",
        }
    }

    fn from_key(key: &str) -> Option<Col> {
        Self::ALL.into_iter().find(|c| c.key() == key)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Process => "Process",
            Self::Command => "Command",
            Self::Path => "Path",
            Self::Memory => "Memory",
            Self::MemPct => "Mem %",
            Self::Virtual => "Virtual",
            Self::Cpu => "CPU %",
            Self::CpuTime => "CPU time",
            Self::Threads => "Threads",
            Self::Fds => "FDs",
            Self::Read => "Read/s",
            Self::Write => "Write/s",
            Self::NetRx => "Net ↓",
            Self::NetTx => "Net ↑",
            Self::User => "User",
            Self::State => "State",
            Self::Nice => "Nice",
            Self::Priority => "Priority",
            Self::Started => "Started",
            Self::Pid => "PID",
            Self::Ppid => "PPID",
        }
    }

    /// The fixed width, or for a column that flexes to fill, its minimum.
    fn width(self) -> f32 {
        match self {
            Self::Process | Self::Command => 220.,
            Self::Path => 240.,
            Self::Memory => 96.,
            Self::MemPct => 72.,
            Self::Virtual => 92.,
            Self::Cpu => 72.,
            Self::CpuTime => 88.,
            Self::Threads => 80.,
            Self::Fds => 64.,
            Self::Read | Self::Write => 88.,
            Self::NetRx | Self::NetTx => 84.,
            Self::User => 104.,
            Self::State => 64.,
            Self::Nice => 60.,
            Self::Priority => 80.,
            Self::Started => 88.,
            Self::Pid | Self::Ppid => 76.,
        }
    }

    fn flexes(self) -> bool {
        matches!(self, Self::Process | Self::Command)
    }

    fn numeric(self) -> bool {
        !matches!(
            self,
            Self::Process | Self::Command | Self::Path | Self::User | Self::State
        )
    }
}

/// Widths for `cols` in a table `total` wide, column widths scaled by `scale` for the text size: fixed columns keep theirs, Process and Command share the rest.
pub fn widths(cols: &[Col], total: f32, scale: f32) -> Vec<f32> {
    let fixed: f32 = cols.iter().filter(|c| !c.flexes()).map(|c| c.width() * scale).sum();
    let spare = (total - fixed).max(0.);
    let both = cols.contains(&Col::Process) && cols.contains(&Col::Command);
    cols.iter()
        .map(|&c| {
            let share = match c {
                Col::Process if both => 0.4,
                Col::Command if both => 0.6,
                _ => 1.,
            };
            if c.flexes() {
                (spare * share).max(c.width() * scale).floor()
            } else {
                c.width() * scale
            }
        })
        .collect()
}

/// The first click on a column sorts it, largest or A first; a second click reverses it.
pub fn toggle_sort((sort, descending): (Col, bool), clicked: Col) -> (Col, bool) {
    if clicked == sort {
        (sort, !descending)
    } else {
        (clicked, clicked.numeric())
    }
}

/// Ascending order on `col`; ties fall back to the pid so equal rows never swap places between refreshes.
pub fn compare(a: &Process, b: &Process, col: Col) -> Ordering {
    let lower = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<Vec<_>>();
    let primary = match col {
        Col::Process => lower(&a.comm).cmp(&lower(&b.comm)),
        Col::Command => lower(&a.cmdline).cmp(&lower(&b.cmdline)),
        Col::Path => a.exe.cmp(&b.exe),
        Col::Memory | Col::MemPct => a.rss_bytes.cmp(&b.rss_bytes),
        Col::Virtual => a.vsize_bytes.cmp(&b.vsize_bytes),
        Col::Cpu => a.cpu_pct.total_cmp(&b.cpu_pct),
        Col::CpuTime => a.cpu_time_s.unwrap_or(-1.).total_cmp(&b.cpu_time_s.unwrap_or(-1.)),
        Col::Threads => a.threads.cmp(&b.threads),
        Col::Fds => a.fds.cmp(&b.fds),
        Col::Read => a.read_bps.cmp(&b.read_bps),
        Col::Write => a.write_bps.cmp(&b.write_bps),
        Col::NetRx => a.net_rx_bps.cmp(&b.net_rx_bps),
        Col::NetTx => a.net_tx_bps.cmp(&b.net_tx_bps),
        Col::User => a.user.cmp(&b.user),
        Col::State => a.state.cmp(&b.state),
        Col::Nice => a.nice.cmp(&b.nice),
        Col::Priority => a.priority.cmp(&b.priority),
        Col::Started => a.start_time.cmp(&b.start_time),
        Col::Pid => Ordering::Equal,
        Col::Ppid => a.ppid.cmp(&b.ppid),
    };
    primary.then(a.pid.cmp(&b.pid))
}

/// The columns the user picked, or `None` for the defaults.
pub fn load_columns(prefs: &Prefs) -> Option<Vec<Col>> {
    let keys: Vec<String> = prefs.get(COLUMNS_KEY)?;
    let picked: Vec<Col> = keys.iter().filter_map(|k| Col::from_key(k)).collect();
    // a picked set always holds Process, so an empty list stands for the defaults
    if picked.is_empty() {
        return None;
    }
    Some(
        Col::ALL
            .into_iter()
            .filter(|c| *c == Col::Process || picked.contains(c))
            .collect(),
    )
}

pub fn save_columns(prefs: &mut Prefs, cols: Option<&[Col]>) {
    let keys: Vec<&str> = cols.unwrap_or_default().iter().map(|c| c.key()).collect();
    prefs.set(COLUMNS_KEY, &keys);
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SortPref {
    column: String,
    descending: bool,
}

pub fn load_sort(prefs: &Prefs) -> Option<(Col, bool)> {
    let pref: SortPref = prefs.get(SORT_KEY)?;
    Some((Col::from_key(&pref.column)?, pref.descending))
}

pub fn save_sort(prefs: &mut Prefs, (col, descending): (Col, bool)) {
    let pref = SortPref {
        column: col.key().into(),
        descending,
    };
    prefs.set(SORT_KEY, &pref);
}

/// Whether the user folded the inspector away; it shows only while a process is selected either way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct InspectorPref {
    pub collapsed: bool,
}

pub fn load_inspector(prefs: &Prefs) -> InspectorPref {
    prefs.get(INSPECTOR_KEY).unwrap_or_default()
}

/// The inspector shows for a selection, unless the user folded it away.
pub fn inspector_open(selected: bool, pref: InspectorPref) -> bool {
    selected && !pref.collapsed
}

fn store(f: impl FnOnce(&mut Prefs)) {
    f(&mut prefs().lock().unwrap_or_else(PoisonError::into_inner));
}

fn state_meaning(state: &str) -> &'static str {
    match state {
        "R" => "Running or runnable",
        "S" => "Sleeping until an event",
        "D" => "Waiting on I/O, uninterruptible",
        "Z" => "Zombie: exited, not yet reaped by its parent",
        "T" => "Stopped by a signal",
        "t" => "Stopped by a debugger",
        "X" => "Dead",
        "I" => "Idle kernel thread",
        "P" => "Parked",
        "W" => "Paging or waking",
        "K" => "Wakekill",
        _ => "Unknown state",
    }
}

/// Minutes, seconds and hundredths, or hours, minutes and seconds once past an hour.
pub fn cpu_time(seconds: f64) -> String {
    let centis = (seconds.max(0.) * 100.).round() as u64;
    let (h, m, s, c) = (centis / 360_000, centis / 6000 % 60, centis / 100 % 60, centis % 100);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}.{c:02}")
    }
}

/// The time of day for a process started in the last day, else the date.
fn started(start: u64, now: f64) -> String {
    if now - (start as f64) < 86_400. {
        clock(start as f64, "%H:%M:%S")
    } else {
        clock(start as f64, "%b %-d")
    }
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

pub struct ProcessTable {
    pub model: Model,
    cols: Vec<Col>,
    /// One per column, from `widths`.
    widths: Vec<f32>,
    icons: Arc<IconIndex>,
    me: Option<String>,
    mem_total: Option<u64>,
}

impl TableDelegate for ProcessTable {
    fn columns_count(&self, _: &App) -> usize {
        self.cols.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.model.lines.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let col = self.cols[col_ix];
        let width = self.widths.get(col_ix).copied().unwrap_or_else(|| col.width());
        // cells pad themselves, so the whole header cell takes the click
        Column::new(col.key(), col.label()).width(px(width)).p_0()
    }

    fn render_th(&mut self, col_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let col = self.cols[col_ix];
        let t = theme(cx);
        let sorted = (col == self.model.sort).then_some(self.model.descending);
        div()
            .id(("th", col_ix))
            .size_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(3.))
            .px(rem_px(CELL_PAD))
            .when(col.numeric(), |d| d.justify_end())
            .cursor_pointer()
            .text_size(rem_px(TABLE_TEXT - 1.))
            .font_weight(t.weight_semibold)
            .text_color(t.text)
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(col.label()),
            )
            .children(sorted.map(|descending| {
                Icon::new(if descending {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronUp
                })
                .size(rem_px(12.))
                .flex_shrink_0()
                .text_color(t.text_secondary)
            }))
            .on_click(cx.listener(move |table, _, _, cx| {
                let model = &mut table.delegate_mut().model;
                let sort = toggle_sort((model.sort, model.descending), col);
                (model.sort, model.descending) = sort;
                model.rebuild();
                store(|p| save_sort(p, sort));
                cx.notify();
            }))
    }

    fn render_tr(&mut self, row_ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let row_key = self.model.process(row_ix).map(key);
        let selected = row_key.is_some() && row_key == self.model.selected;
        let tint = theme(cx).selection;
        div()
            .id(("row", row_ix))
            .when(selected, |d| d.bg(tint))
            .on_click(cx.listener(move |table, _, _, cx| {
                table.delegate_mut().model.selected = if selected { None } else { row_key };
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
        let col = self.cols[col_ix];
        let cell = div()
            .size_full()
            .min_w_0()
            .flex()
            .items_center()
            .px(rem_px(CELL_PAD))
            .overflow_hidden();
        let number = |text: String| {
            div()
                .w_full()
                .flex()
                .justify_end()
                .font_features(t.numbers.clone())
                .child(text)
                .into_any_element()
        };
        let text = |text: String, color: Hsla| {
            div()
                .w_full()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        let dash = || {
            div()
                .w_full()
                .flex()
                .when(col.numeric(), |d| d.justify_end())
                .text_color(t.text_tertiary)
                .child("–")
                .into_any_element()
        };
        let unreadable = |id: &'static str| {
            div()
                .id((id, row_ix))
                .w_full()
                .flex()
                .when(col.numeric(), |d| d.justify_end())
                .text_color(t.text_tertiary)
                .child("–")
                .tooltip(|window, cx| Tooltip::new("not permitted").build(window, cx))
                .into_any_element()
        };
        let rate = |v: Option<u64>, id: &'static str| {
            v.map_or_else(|| unreadable(id), |n| number(Unit::BytesPerSec.format(n as f64)))
        };
        let content = match col {
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
                            .size(rem_px(12.))
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
                    div().w(rem_px(12.)).flex_shrink_0().into_any_element()
                };
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(rem_px(6.))
                    .min_w_0()
                    .pl(rem_px(f32::from(line.depth) * INDENT))
                    .when(self.model.hierarchy, |d| d.child(disclosure))
                    .child(app_icon(&p.comm, &p.cmdline, &self.icons, 16., cx))
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
            Col::Command if p.cmdline.is_empty() => dash(),
            Col::Command => text(p.cmdline.clone(), t.text_secondary),
            // kernel threads have no executable at all, which is not a matter of permission
            Col::Path if p.cmdline.is_empty() => dash(),
            Col::Path => p
                .exe
                .clone()
                .map_or_else(|| unreadable("exe"), |exe| text(exe, t.text_secondary)),
            Col::Memory => number(bytes(p.rss_bytes as f64)),
            Col::MemPct => self.mem_total.filter(|&total| total > 0).map_or_else(dash, |total| {
                number(memory_share(100. * p.rss_bytes as f64 / total as f64))
            }),
            Col::Virtual => p.vsize_bytes.map_or_else(dash, |v| number(bytes(v as f64))),
            Col::Cpu => number(format!("{:.1}%", p.cpu_pct)),
            Col::CpuTime => p.cpu_time_s.map_or_else(dash, |s| number(cpu_time(s))),
            Col::Threads => number(p.threads.to_string()),
            Col::Fds => p.fds.map_or_else(|| unreadable("fds"), |n| number(n.to_string())),
            Col::Read => rate(p.read_bps, "read"),
            Col::Write => rate(p.write_bps, "write"),
            Col::NetRx => rate(p.net_rx_bps, "net-rx"),
            Col::NetTx => rate(p.net_tx_bps, "net-tx"),
            Col::User => p.user.clone().map_or_else(dash, |u| text(u, t.text_secondary)),
            Col::State => {
                let meaning = state_meaning(&p.state);
                div()
                    .id(("state", row_ix))
                    .child(p.state.clone())
                    .tooltip(move |window, cx| Tooltip::new(meaning).build(window, cx))
                    .into_any_element()
            }
            Col::Nice => p.nice.map_or_else(dash, |n| number(n.to_string())),
            Col::Priority => p.priority.map_or_else(dash, |n| number(n.to_string())),
            Col::Started if p.start_time == 0 => dash(),
            Col::Started => number(started(p.start_time, wall_clock())),
            Col::Pid => number(p.pid.to_string()),
            Col::Ppid => number(p.ppid.to_string()),
        };
        cell.child(content).into_any_element()
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
            .when_some(p.exe.clone(), |menu, exe| menu.item(copy("Copy path", exe)))
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
            .pt(rem_px(40.))
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
pub(crate) struct Icons(pub(crate) Arc<IconIndex>);

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

/// Below 1% a second decimal, so a small process on a large machine does not read as nothing.
fn memory_share(pct: f64) -> String {
    if pct >= 1. {
        format!("{pct:.1}%")
    } else if pct >= 0.01 {
        format!("{pct:.2}%")
    } else if pct > 0. {
        "<0.01%".into()
    } else {
        "0%".into()
    }
}

/// The cards' history read for one range.
struct StoredCards {
    range: Range,
    cards: HashMap<Card, Vec<Bucket>>,
}

/// History read for one range and one selected process.
struct Stored {
    range: Range,
    key: Key,
    /// `None` when the daemon has not recorded the process yet.
    proc_id: Option<i64>,
    fields: HashMap<ProcField, Vec<Bucket>>,
    /// Earlier runs of the same program in the range, summed, and how many there were.
    earlier: HashMap<ProcField, Vec<Bucket>>,
    earlier_runs: usize,
}

/// Same name and command line, started before `start`, and seen inside the range.
fn earlier_runs(
    history: &History,
    name: &(String, String),
    start: u64,
    from: i64,
    current: Option<i64>,
) -> Result<Vec<i64>> {
    let (comm, cmdline) = name;
    Ok(history
        .find_procs(comm)?
        .into_iter()
        .filter(|p| {
            &p.comm == comm
                && p.cmdline.as_deref().unwrap_or_default() == cmdline
                && Some(p.id) != current
                && p.start_time < start as i64
                && p.last_t >= from
        })
        .map(|p| p.id)
        .collect())
}

fn load_cards(range: Range, to: i64) -> Result<StoredCards> {
    let history = History::open(&db_path())?;
    let from = to - range.span_s();
    let mut cards = HashMap::new();
    for card in Card::ALL {
        cards.insert(card, card.source().query(&history, from, to, BUCKETS)?);
    }
    Ok(StoredCards { range, cards })
}

fn load(range: Range, to: i64, selected: Key, name: (String, String)) -> Result<Stored> {
    let history = History::open(&db_path())?;
    let from = to - range.span_s();
    let (pid, start) = selected;
    let proc_id = history.proc_id(pid, start)?;
    let mut fields = HashMap::new();
    if let Some(id) = proc_id {
        for field in FIELDS {
            fields.insert(field, history.proc_series(id, field, from, to, BUCKETS)?);
        }
    }
    let ids = earlier_runs(&history, &name, start, from, proc_id)?;
    let mut earlier = HashMap::new();
    if !ids.is_empty() {
        for field in [ProcField::Rss, ProcField::Cpu, ProcField::Fds] {
            earlier.insert(field, history.procs_sum(&ids, field, from, to, BUCKETS)?);
        }
    }
    Ok(Stored {
        range,
        key: selected,
        proc_id,
        fields,
        earlier,
        earlier_runs: ids.len(),
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
    me: WeakEntity<Self>,
    live: Entity<Live>,
    feed: Entity<ProcessFeed>,
    table: Option<Entity<TableState<ProcessTable>>>,
    /// The delegate until the first render, which has the window the table and search field need.
    pending: Option<ProcessTable>,
    search: Option<Entity<InputState>>,
    /// The cards' range.
    range: Range,
    cards_stored: Option<StoredCards>,
    cards_loading: bool,
    cards_attempt: Option<(Range, f64)>,
    /// The inspector's range, chosen apart from the cards'.
    proc_range: Range,
    stored: Option<Stored>,
    loading: bool,
    attempt: Option<(Range, Key, f64)>,
    error: Option<SharedString>,
    /// The selected process as last scanned; kept after it exits.
    inspected: Option<Process>,
    /// Live scans of the selected process since it was selected, to reach past the last recorded point.
    trail: Vec<(f64, Process)>,
    cards: HashMap<Card, Rc<[Bucket]>>,
    fields: HashMap<ProcField, Rc<[Bucket]>>,
    span: (f64, f64),
    width_s: i64,
    proc_span: (f64, f64),
    proc_width_s: i64,
    version: u64,
    charts: [Rc<RefCell<ChartState>>; 4],
    cpu_meta: Option<CpuMeta>,
    /// A process another screen asked to show, selected once the table holds it.
    focus: Option<Focus>,
    /// The columns the user picked; `None` follows the defaults.
    columns: Option<Vec<Col>>,
    panel: InspectorPref,
    inspector: Entity<Sidebar>,
    keys: FocusHandle,
    #[cfg(debug_assertions)]
    dev_select: Option<String>,
}

/// Toggles `col` in `cols`, keeping display order; Process stays, as it holds the tree.
pub fn toggle_column(cols: &[Col], col: Col) -> Vec<Col> {
    let on = cols.contains(&col);
    Col::ALL
        .into_iter()
        .filter(|&c| c == Col::Process || if c == col { !on } else { cols.contains(&c) })
        .collect()
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
        let (columns, sort, panel) = {
            let prefs = prefs().lock().unwrap_or_else(PoisonError::into_inner);
            (load_columns(&prefs), load_sort(&prefs), load_inspector(&prefs))
        };
        let mut model = Model::new(cx.try_global::<HierarchyMode>().is_some_and(|h| h.0));
        if let Some((col, descending)) = sort {
            (model.sort, model.descending) = (col, descending);
        }
        let inspector = cx.new(|_| Sidebar::on_demand("processes.right", Side::Right, INSPECTOR));
        cx.subscribe(&inspector, |this: &mut Self, sidebar, _: &Toggled, cx| {
            // its own hide button folds it away just as the toolbar's toggle does
            let collapsed = sidebar.read(cx).collapsed();
            if collapsed == this.inspector_open() {
                this.panel.collapsed = collapsed;
                this.save_inspector();
                cx.notify();
            }
        })
        .detach();
        let mut this = Self {
            me: cx.weak_entity(),
            live,
            feed,
            table: None,
            pending: Some(ProcessTable {
                model,
                icons,
                me: current_user(),
                cols: Vec::new(),
                widths: Vec::new(),
                mem_total: None,
            }),
            columns,
            panel,
            inspector,
            keys: cx.focus_handle(),
            search: None,
            range: remembered(Tab::Processes, cx),
            cards_stored: None,
            cards_loading: false,
            cards_attempt: None,
            proc_range: remembered_in(Tab::Processes, INSPECTOR_RANGE, cx),
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
            proc_span: (0., 0.),
            proc_width_s: 1,
            version: 0,
            charts: std::array::from_fn(|_| ChartState::with_cursor(cursor.clone())),
            cpu_meta: None,
            focus: take_focus(cx),
            #[cfg(debug_assertions)]
            dev_select: std::env::var("SYSMAN_DEV_SELECT").ok(),
        };
        #[cfg(debug_assertions)]
        if std::env::var_os("SYSMAN_DEV_HIERARCHY").is_some() {
            this.with_table(cx, |table| table.model.hierarchy = true);
        }
        #[cfg(debug_assertions)]
        if let Ok(keys) = std::env::var("SYSMAN_DEV_PROC_COLUMNS") {
            this.columns = Some(match keys.as_str() {
                "all" => Col::ALL.to_vec(),
                keys => Col::ALL
                    .into_iter()
                    .filter(|c| *c == Col::Process || keys.split(',').any(|k| k == c.key()))
                    .collect(),
            });
        }
        #[cfg(debug_assertions)]
        if std::env::var_os("SYSMAN_DEV_INSPECTOR_COLLAPSED").is_some() {
            this.panel.collapsed = true;
        }
        #[cfg(debug_assertions)]
        if let Ok(name) = std::env::var("SYSMAN_DEV_INSPECTOR_RANGE") {
            this.proc_range = Range::ALL
                .into_iter()
                .find(|r| r.label().eq_ignore_ascii_case(&name))
                .unwrap_or(this.proc_range);
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
            && !self.cards_loading
            && !self.loading
            && self.cards_attempt.is_some_and(|(range, _)| range == self.range)
            && selected.is_none_or(|selected| {
                self.attempt
                    .is_some_and(|(range, key, _)| range == self.proc_range && key == selected)
            })
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
        self.apply_focus(cx);
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

    fn apply_focus(&mut self, cx: &mut Context<Self>) {
        let Some(Focus::Process { comm, instance }) = self.focus.take() else {
            return;
        };
        let line = self.with_table(cx, |d| {
            let m = &mut d.model;
            let line = instance
                .and_then(|k| m.lines.iter().position(|l| key(&m.table[l.row]) == k))
                .or_else(|| {
                    let named = m.lines.iter().enumerate().filter(|(_, l)| m.table[l.row].comm == comm);
                    named.max_by_key(|(_, l)| m.table[l.row].start_time).map(|(i, _)| i)
                })?;
            m.selected = m.process(line).map(key);
            Some(line)
        });
        if let (Some(line), Some(table)) = (line, &self.table) {
            table.update(cx, |state, cx| state.scroll_to_row(line, cx));
        }
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
        self.sync_inspector(cx);
        self.refresh(cx);
    }

    fn inspector_open(&self) -> bool {
        inspector_open(self.inspected.is_some(), self.panel)
    }

    fn sync_inspector(&mut self, cx: &mut Context<Self>) {
        let open = self.inspector_open();
        self.inspector.update(cx, |s, cx| s.set_collapsed(!open, cx));
    }

    fn deselect(&mut self, cx: &mut Context<Self>) {
        self.with_table(cx, |table| table.model.selected = None);
        self.sync_selection(cx);
    }

    fn set_proc_range(&mut self, range: Range, cx: &mut Context<Self>) {
        if range != self.proc_range {
            self.proc_range = range;
            remember_in(Tab::Processes, INSPECTOR_RANGE, range, cx);
            self.refresh(cx);
        }
    }

    fn set_column(&mut self, col: Col, cx: &mut Context<Self>) {
        let cols = toggle_column(&self.delegate(cx).cols, col);
        store(|p| save_columns(p, Some(&cols)));
        self.columns = Some(cols);
        cx.notify();
    }

    fn reset_columns(&mut self, cx: &mut Context<Self>) {
        store(|p| save_columns(p, None));
        self.columns = None;
        cx.notify();
    }

    fn toggle_inspector(&mut self, cx: &mut Context<Self>) {
        self.panel.collapsed = !self.panel.collapsed;
        self.save_inspector();
        self.sync_inspector(cx);
        cx.notify();
    }

    fn save_inspector(&self) {
        store(|p| p.set(INSPECTOR_KEY, &self.panel));
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
        let cards_stale = self.cards_attempt.is_none_or(|(range, at)| {
            range != self.range
                || self
                    .live
                    .read(cx)
                    .reload_due(at, now, self.range.span_s() as f64, RELOAD_S)
        });
        if cards_stale && !self.cards_loading {
            self.load_cards(now, cx);
        }
        if let Some(p) = &self.inspected {
            let selected = key(p);
            // The inspected process's series have no live rings to fill a pause.
            let stale = self
                .attempt
                .is_none_or(|(range, key, at)| range != self.proc_range || key != selected || now - at > RELOAD_S);
            if stale && !self.loading {
                let name = (p.comm.clone(), p.cmdline.clone());
                self.load(now, selected, name, cx);
            }
        }
        self.merge(now, cx);
        cx.notify();
    }

    fn load_cards(&mut self, now: f64, cx: &mut Context<Self>) {
        self.cards_loading = true;
        self.cards_attempt = Some((self.range, now));
        let range = self.range;
        let task = cx
            .background_executor()
            .spawn(async move { load_cards(range, now as i64) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.cards_loading = false;
                match result {
                    Ok(stored) => {
                        this.error = None;
                        this.cards_stored = Some(stored);
                    }
                    Err(e) => this.error = Some(format!("History unavailable: {e}").into()),
                }
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    fn load(&mut self, now: f64, selected: Key, name: (String, String), cx: &mut Context<Self>) {
        self.loading = true;
        self.attempt = Some((self.proc_range, selected, now));
        let range = self.proc_range;
        let task = cx
            .background_executor()
            .spawn(async move { load(range, now as i64, selected, name) });
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
        let stored = self.cards_stored.as_ref().filter(|s| s.range == self.range);
        for card in Card::ALL {
            let history = stored.and_then(|s| s.cards.get(&card)).map_or(&[][..], Vec::as_slice);
            let merged = card.source().top_up(history, live, from, self.width_s);
            self.cards.insert(card, merged.into());
        }
        self.span = (from as f64, now);
        let from = to - self.proc_range.span_s();
        self.proc_width_s = bucket_width(from, to, BUCKETS);
        let selected = self.inspected.as_ref().map(key);
        let stored = self
            .stored
            .as_ref()
            .filter(|s| s.range == self.proc_range && Some(s.key) == selected);
        let start = self.trail.first().map_or(f64::INFINITY, |(t, _)| *t);
        for field in FIELDS {
            let history = stored.and_then(|s| s.fields.get(&field)).map_or(&[][..], Vec::as_slice);
            let points = self
                .trail
                .iter()
                .filter_map(|(t, p)| Some(((t * 1000.).round() as i64, field_value(p, field)?)));
            let merged = top_up(history, start, points, from, self.proc_width_s);
            self.fields.insert(field, merged.into());
        }
        self.proc_span = (from as f64, now);
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
            ("processes-spark", card as usize),
            Self::data(&self.cards, card),
            color,
            self.span,
            self.width_s as f64,
            gap,
        )
    }
}

/// A captioned monospace value with a Copy button, or `absent` dimmed when there is nothing to copy.
fn copyable(id: &'static str, label: &str, value: Option<String>, absent: &str, cx: &App) -> impl IntoElement {
    let t = theme(cx);
    let text = div().text_size(t.caption).font_family(t.mono.clone());
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .child(caption(label.to_owned(), cx))
                .child(div().flex_1())
                .when_some(value.clone(), |d, value| {
                    d.child(
                        Button::new(id)
                            .ghost()
                            .xsmall()
                            .icon(Icon::new(IconName::Copy))
                            .tooltip(format!("Copy {}", label.to_lowercase()))
                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(value.clone()))),
                    )
                }),
        )
        .child(match value {
            Some(value) => text.text_color(t.text).child(value),
            None => text.text_color(t.text_tertiary).child(absent.to_owned()),
        })
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
                .font_weight(t.weight_semibold)
                .font_features(t.numbers.clone())
                .child(value),
        )
        .child(div().flex_1())
        .children(detail.map(|d| caption(d, cx).font_features(t.numbers.clone())))
}

impl Processes {
    fn expand(&self, card: Card) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
        opener(self.me.clone(), card, |this, card, cx| Some(this.detail(*card, cx)))
    }

    fn detail(&self, card: Card, cx: &App) -> Detail {
        let t = theme(cx);
        let (title, icon, unit, y, color, meaning, method) = match card {
            Card::Cpu => (
                "CPU usage",
                IconName::Cpu,
                Unit::Percent,
                YScale::Fixed(0., 100.),
                t.green,
                "The share of all logical CPUs that was busy. A long run near 100% means work is queueing for \
                 the processor.",
                "From /proc/stat: the change in busy time across all CPUs between two samples, divided by the \
                 change in total time. Busy is everything except idle and I/O wait.",
            ),
            Card::Load => (
                "Load average",
                IconName::Activity,
                Unit::Count,
                YScale::Zero,
                t.orange,
                "The average number of tasks running or waiting to run, plus those in uninterruptible sleep, \
                 usually waiting for disk. Above the number of logical CPUs, work is queueing.",
                "The kernel's exponentially damped 1-minute load average from /proc/loadavg, read at each sample.",
            ),
            Card::Temp => (
                "CPU temperature",
                IconName::Thermometer,
                Unit::Celsius,
                YScale::Fit(10.),
                t.orange,
                "The processor package temperature. Sustained readings near its limit make it lower its clocks.",
                "The CPU package sensor (coretemp or k10temp) from /sys/class/hwmon, in degrees Celsius.",
            ),
        };
        Detail {
            title: title.into(),
            icon,
            unit,
            y,
            fill: card == Card::Cpu,
            lines: vec![Series {
                label: title.into(),
                color,
                source: card.source(),
            }],
            rules: vec![],
            ticks: None,
            meaning,
            method,
            range: self.range,
            live: self.live.clone(),
        }
    }

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
            .gap(rem_px(10.))
            .children(groups.groups.iter().map(|(label, color, cpus)| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(rem_px(5.))
                    .text_size(t.small_caption)
                    .text_color(t.text_secondary)
                    .font_features(t.numbers.clone())
                    .child(div().size(rem_px(8.)).rounded(px(2.)).bg(*color))
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
            .gap(rem_px(10.))
            .px(t.page_padding)
            .pt(rem_px(14.))
            .pb(rem_px(10.))
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
                    .gap(rem_px(10.))
                    .child(
                        card("card-cpu", "CPU usage", cpu_color, cx)
                            .on_click(self.expand(Card::Cpu))
                            .child(value_row(total.map_or("–".into(), |v| format!("{v:.0}%")), split, cx))
                            .child(with_peak(
                                self.spark(Card::Cpu, cpu_color).fixed(100., "100%"),
                                peak(Card::Cpu, Unit::Percent),
                            )),
                    )
                    .child(
                        card("card-cores", "Cores", t.blue, cx)
                            .on_click(self.expand(Card::Cpu))
                            .child(core_bars(bars, px(58.), cx))
                            .child(core_legend),
                    )
                    .child(
                        card("card-load", "Load average", t.orange, cx)
                            .on_click(self.expand(Card::Load))
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
                            .on_click(self.expand(Card::Temp))
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
        let collapsed = self.panel.collapsed;
        let this = cx.entity().downgrade();
        let search = self.search.as_ref().map(|search| {
            div()
                .w(rem_px(380.))
                .min_w(rem_px(160.))
                .h(CONTROL_HEIGHT)
                .flex()
                .items_center()
                .px(rem_px(4.))
                .rounded(rem_px(8.))
                .bg(t.window)
                .border_1()
                .border_color(t.control_border)
                .child(
                    Input::new(search)
                        .appearance(false)
                        .cleanable(true)
                        .prefix(Icon::new(IconName::Search).small().text_color(t.text_secondary)),
                )
        });
        let button = |id: &'static str| pill(Button::new(id).ghost().small(), t).font_weight(t.weight_medium);
        div()
            .flex_shrink_0()
            .h(rem_px(TOOLBAR_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .gap(rem_px(8.))
            .px(rem_px(12.))
            .bg(t.panel)
            .border_t_1()
            .border_b_1()
            .border_color(t.hairline)
            .children(search)
            .child(div().flex_1())
            .child(
                button("hierarchy")
                    .px(rem_px(12.))
                    .icon(Icon::new(IconName::ListTree))
                    .label("Hierarchy")
                    .when(hierarchy, |b| b.bg(t.strip_active).border_color(t.strip_border))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_hierarchy(cx))),
            )
            .child(
                button("columns")
                    .px(rem_px(12.))
                    .icon(Icon::new(IconName::Columns3))
                    .label("Columns")
                    .dropdown_caret(true)
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, cx| {
                        let Some(screen) = this.upgrade() else {
                            return menu;
                        };
                        let shown = screen.read(cx).delegate(cx).cols.clone();
                        for col in Col::ALL {
                            let screen = screen.downgrade();
                            menu = menu.item(
                                PopupMenuItem::new(col.label())
                                    .checked(shown.contains(&col))
                                    .disabled(col == Col::Process)
                                    .on_click(move |_, _, cx| {
                                        screen.update(cx, |this, cx| this.set_column(col, cx)).ok();
                                    }),
                            );
                        }
                        let screen = screen.downgrade();
                        menu.separator()
                            .item(PopupMenuItem::new("Reset to default").on_click(move |_, _, cx| {
                                screen.update(cx, |this, cx| this.reset_columns(cx)).ok();
                            }))
                    }),
            )
            .child(
                button("inspector-toggle")
                    .w(CONTROL_HEIGHT)
                    .icon(Icon::new(if collapsed {
                        IconName::PanelRightOpen
                    } else {
                        IconName::PanelRightClose
                    }))
                    .when(!collapsed, |b| b.bg(t.strip_active).border_color(t.strip_border))
                    .tooltip(if collapsed {
                        "Show details of the selected process"
                    } else {
                        "Hide details"
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_inspector(cx))),
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
            .gap(rem_px(6.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(rem_px(7.))
                    .child(Icon::new(icon).small().text_color(t.text_secondary))
                    .child(
                        div()
                            .text_size(t.panel_title)
                            .font_weight(t.weight_semibold)
                            .child(title.to_owned()),
                    )
                    .child(div().flex_1())
                    .children(legend),
            )
            .child(chart)
            .child(caption(note.to_owned(), cx))
    }

    fn chart(&self, i: usize) -> TrendChart {
        let gap = (self.proc_width_s as f64 * 1.5).max(RECORD_INTERVAL_S * 2.5);
        TrendChart::new(
            self.charts[i].clone(),
            self.version,
            self.proc_span,
            self.proc_width_s as f64,
            gap,
        )
        .relative_axis()
        .y(YScale::Zero)
        .height(px(CHART_HEIGHT))
    }

    fn inspector(&self, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let close = this.clone();
        let cx: &App = cx;
        let t = theme(cx);
        let body = div().flex().flex_col().gap(rem_px(16.)).p(rem_px(16.));
        // only while sliding out after a deselect
        let Some(p) = &self.inspected else {
            return body.into_any_element();
        };
        let icons = self.delegate(cx).icons.clone();
        let running = self.delegate(cx).model.selected_process().is_some();
        let stored = self
            .stored
            .as_ref()
            .filter(|s| s.range == self.proc_range && s.key == key(p));
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
                .gap(rem_px(8.))
                .text_size(t.caption)
                .child(
                    div()
                        .w(rem_px(64.))
                        .text_color(t.text_secondary)
                        .child(label.to_owned()),
                )
                .child(div().flex_1().min_w_0().font_features(t.numbers.clone()).child(value))
        };
        let details = hairline(
            div()
                .flex()
                .flex_col()
                .gap(rem_px(6.))
                .py(rem_px(10.))
                .px(rem_px(12.))
                .rounded(t.card_radius)
                .bg(t.card)
                .border_color(t.hairline),
            t.hairline_width,
        )
        .child(kv("User", p.user.clone().unwrap_or_else(|| "–".into())))
        .child(kv("Started", clock(p.start_time as f64, "%b %-d %H:%M:%S")))
        .child(kv("State", format!("{} · {}", p.state, state_meaning(&p.state))))
        .child(kv(
            "Nice",
            match (p.nice, p.priority) {
                (Some(nice), Some(priority)) => format!("{nice} · priority {priority}"),
                _ => "–".into(),
            },
        ))
        .child(kv("CPU time", p.cpu_time_s.map_or_else(|| "–".into(), cpu_time)))
        .child(kv(
            "Virtual",
            p.vsize_bytes.map_or_else(|| "–".into(), |v| bytes(v as f64)),
        ))
        .child(copyable(
            "copy-path",
            "Path",
            p.exe.clone(),
            if p.cmdline.is_empty() {
                "–"
            } else {
                "– (not permitted)"
            },
            cx,
        ))
        .child(copyable(
            "copy-command",
            "Command line",
            Some(p.cmdline.clone()).filter(|c| !c.is_empty()),
            &format!("[{}]", p.comm),
            cx,
        ));
        let swatch = |color: Hsla, label: &'static str| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .text_size(t.small_caption)
                .text_color(t.text_secondary)
                .child(div().size(rem_px(6.)).rounded_full().bg(color))
                .child(label)
        };
        let field = |f: ProcField| Self::data(&self.fields, f);
        let earlier_of = self
            .stored
            .as_ref()
            .filter(|s| s.range == self.proc_range && s.key == key(p) && s.earlier_runs > 0);
        let earlier = |f: ProcField| -> Rc<[Bucket]> {
            earlier_of
                .and_then(|s| s.earlier.get(&f))
                .map_or_else(|| Rc::from(Vec::new()), |b| Rc::from(b.as_slice()))
        };
        let runs = earlier_of.map_or(0, |s| s.earlier_runs);
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
                .gap(rem_px(10.))
                .child(app_icon(&p.comm, &p.cmdline, &icons, 32., cx))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(t.card_value)
                                .font_weight(t.weight_semibold)
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(p.comm.clone()),
                        )
                        .child(caption(format!("PID {}", p.pid), cx).font_features(t.numbers.clone())),
                )
                .child(div().flex_1())
                .child(
                    Button::new("inspector-close")
                        .ghost()
                        .small()
                        .icon(Icon::new(IconName::X))
                        .tooltip("Close (Esc)")
                        .on_click(move |_, _, cx| {
                            close.update(cx, |this, cx| this.deselect(cx)).ok();
                        }),
                ),
        )
        .child(details)
        .when_some(status, |d, s| d.child(caption(s, cx).text_color(t.orange)))
        .child(range_picker(
            self.proc_range,
            move |range, _, cx| {
                this.update(cx, |p, cx| p.set_proc_range(range, cx)).ok();
            },
            cx,
        ))
        .when(runs > 0, |d| {
            let runs = if runs == 1 {
                "1 earlier run".to_owned()
            } else {
                format!("{runs} earlier runs")
            };
            d.child(caption(
                format!("Dashed lines: {runs} of the same command in this range, summed."),
                cx,
            ))
        })
        .child(
            self.chart_section(
                IconName::MemoryStick,
                "Memory (RSS)",
                self.chart(0)
                    .dashed(earlier(ProcField::Rss), t.blue.opacity(0.5), "Earlier runs")
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
                    .dashed(earlier(ProcField::Cpu), t.green.opacity(0.5), "Earlier runs")
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
                    .dashed(earlier(ProcField::Fds), t.purple.opacity(0.5), "Earlier runs")
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
                        .gap(rem_px(10.))
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
                    .col_selectable(false)
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
        let sample = self.live.read(cx).latest.clone();
        let helper_running = helper::running(sample.as_deref());
        let cols = self
            .columns
            .clone()
            .unwrap_or_else(|| Col::defaults(helper::net_columns(&self.delegate(cx).model.table, helper_running)));
        let inspector_width = self.inspector.read(cx).width();
        let widths = widths(
            &cols,
            f32::from(window.viewport_size().width - inspector_width) - SCROLLBAR,
            window.rem_size() / px(16.),
        );
        let mem_total = sample.as_ref().and_then(|s| s.memory.as_ref()).map(|m| m.total_bytes);
        if let Some(table) = &self.table {
            let delegate = table.read(cx).delegate();
            if delegate.widths != widths || delegate.cols != cols || delegate.mem_total != mem_total {
                let layout = delegate.widths != widths || delegate.cols != cols;
                table.update(cx, |state, cx| {
                    let delegate = state.delegate_mut();
                    delegate.widths = widths;
                    delegate.cols = cols;
                    delegate.mem_total = mem_total;
                    if layout {
                        state.refresh(cx);
                    }
                });
            }
        }
        let toolbar = self.toolbar(cx).into_any_element();
        let inspector = sidebar::showing(&self.inspector, cx).then(|| {
            let t = theme(cx);
            div()
                .id("process-inspector")
                .size_full()
                .overflow_y_scroll()
                .border_l_1()
                .border_t_1()
                .rounded_tl(t.panel_radius)
                .border_color(t.hairline)
                .bg(t.panel)
                .child(self.inspector(cx))
                .into_any_element()
        });
        let inspector = sidebar::sidebar(&self.inspector, inspector, window, cx);
        let t = theme(cx);
        let table = self.table.clone().expect("built above");
        let keys = self.keys.clone();
        div()
            .size_full()
            .flex()
            .flex_row()
            .track_focus(&self.keys)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, _, cx| {
                if e.keystroke.key == "escape" && this.inspected.is_some() {
                    this.deselect(cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(self.header(sample.as_deref(), cx))
                    .child(toolbar)
                    .when(!helper_running, |d| {
                        d.child(
                            div()
                                .px(rem_px(12.))
                                .py(rem_px(3.))
                                .border_b_1()
                                .border_color(t.hairline)
                                .child(helper_hint(
                                    "processes-enable",
                                    "Disk I/O and open files of other users' processes need the privileged helper",
                                    cx,
                                )),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .text_size(rem_px(TABLE_TEXT))
                            .font_features(t.numbers.clone())
                            // so Escape reaches this screen after a row is clicked
                            .on_mouse_down(MouseButton::Left, move |_, window, cx| keys.focus(window, cx))
                            .child(
                                DataTable::new(&table)
                                    .stripe(true)
                                    .bordered(false)
                                    .with_size(Size::Small),
                            ),
                    ),
            )
            .child(inspector)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_process_on_a_large_machine_still_shows_its_memory_share() {
        let pct = 100. * (47 << 20) as f64 / (128u64 << 30) as f64;
        assert_eq!(memory_share(pct), "0.04%", "47 MB of 128 GB is not 0.0%");
        assert_eq!(memory_share(0.004), "<0.01%");
        assert_eq!(memory_share(0.), "0%", "kernel threads own no memory");
        assert_eq!(memory_share(12.34), "12.3%");
    }

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
            ..Default::default()
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
    fn net_columns_sit_after_fds_and_sort_absent_rates_lowest() {
        assert!(!Col::defaults(false).contains(&Col::NetRx));
        let cols = Col::defaults(true);
        let fds = cols.iter().position(|&c| c == Col::Fds).unwrap();
        assert_eq!(cols[fds + 1..fds + 3], [Col::NetRx, Col::NetTx]);
        let mut busy = proc(1, 0, "a", 1);
        busy.net_tx_bps = Some(10);
        assert_eq!(
            compare(&busy, &proc(2, 0, "b", 1), Col::NetTx),
            Ordering::Greater,
            "a process the helper has no rate for sorts below any rate"
        );
    }

    #[test]
    fn a_header_click_sorts_numbers_largest_first_and_text_a_first_then_reverses() {
        let start = (Col::Memory, true);
        let cpu = toggle_sort(start, Col::Cpu);
        assert_eq!(cpu, (Col::Cpu, true), "the busiest process is what one looks for first");
        assert_eq!(toggle_sort(cpu, Col::Cpu), (Col::Cpu, false));
        assert_eq!(
            toggle_sort((Col::Cpu, false), Col::Cpu),
            (Col::Cpu, true),
            "a third click flips back"
        );
        let user = toggle_sort(cpu, Col::User);
        assert_eq!(user, (Col::User, false), "names read A to Z");
        assert_eq!(toggle_sort(user, Col::User), (Col::User, true));
        assert_eq!(
            toggle_sort((Col::Cpu, false), Col::Started),
            (Col::Started, true),
            "newest first"
        );
    }

    #[test]
    fn picked_columns_and_sort_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        let mut prefs = Prefs::load(path.clone());
        assert_eq!(load_columns(&prefs), None, "no choice yet: the defaults");
        let cols = toggle_column(&toggle_column(&Col::defaults(false), Col::Fds), Col::Ppid);
        save_columns(&mut prefs, Some(&cols));
        save_sort(&mut prefs, (Col::CpuTime, false));

        let mut prefs = Prefs::load(path.clone());
        let loaded = load_columns(&prefs).unwrap();
        assert_eq!(loaded, cols);
        assert!(!loaded.contains(&Col::Fds) && loaded.contains(&Col::Ppid));
        assert_eq!(load_sort(&prefs), Some((Col::CpuTime, false)));

        save_columns(&mut prefs, None);
        assert_eq!(
            load_columns(&Prefs::load(path)),
            None,
            "a reset follows the defaults again, so a later helper still brings the Net pair"
        );
    }

    #[test]
    fn the_process_column_cannot_be_hidden_and_unknown_names_are_dropped() {
        assert_eq!(
            toggle_column(&[Col::Process, Col::Pid], Col::Process),
            [Col::Process, Col::Pid],
            "it holds the tree and the icon"
        );
        assert_eq!(
            toggle_column(&[Col::Process, Col::Pid], Col::Memory),
            [Col::Process, Col::Memory, Col::Pid],
            "display order, not click order"
        );
        let dir = tempfile::tempdir().unwrap();
        let mut prefs = Prefs::load(dir.path().join("ui.toml"));
        // a file from a newer or older version may name columns this one does not know
        prefs.set(COLUMNS_KEY, &["pid", "gone", "user"]);
        assert_eq!(load_columns(&prefs), Some(vec![Col::Process, Col::User, Col::Pid]));
        prefs.set(COLUMNS_KEY, &["gone"]);
        assert_eq!(load_columns(&prefs), None);
    }

    #[test]
    fn the_inspector_shows_only_for_a_selection_and_remembers_being_folded() {
        let open = InspectorPref { collapsed: false };
        let folded = InspectorPref { collapsed: true };
        // with nothing selected an inspector could only be an empty pane
        assert!(!inspector_open(false, open));
        assert!(inspector_open(true, open));
        assert!(
            !inspector_open(true, folded),
            "a fold outlives the selection that showed it"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        let mut prefs = Prefs::load(path.clone());
        assert_eq!(load_inspector(&prefs), open);
        prefs.set(INSPECTOR_KEY, &folded);
        assert_eq!(load_inspector(&Prefs::load(path.clone())), folded);
        // a file written when the width was stored here still loads; the sidebar keeps the width now
        std::fs::write(&path, "[\"processes.inspector\"]\nwidth = 500.0\ncollapsed = true\n").unwrap();
        assert_eq!(load_inspector(&Prefs::load(path)), folded);
    }

    #[test]
    fn process_and_command_share_the_spare_width() {
        let cols = [Col::Process, Col::Memory, Col::Pid];
        let w = widths(&cols, 1000., 1.);
        assert_eq!(w.iter().sum::<f32>(), 1000., "the table fills its width");
        assert_eq!(w[1..], [Col::Memory.width(), Col::Pid.width()]);
        let w = widths(&[Col::Process, Col::Command, Col::Pid], 1076., 1.);
        assert_eq!(w, [400., 600., 76.]);
        let w = widths(&cols, 100., 1.);
        assert_eq!(
            w[0],
            Col::Process.width(),
            "too narrow: scroll rather than crush the name"
        );
    }

    #[test]
    fn cpu_time_reads_like_a_stopwatch() {
        assert_eq!(cpu_time(0.), "0:00.00");
        assert_eq!(cpu_time(83.456), "1:23.46");
        assert_eq!(cpu_time(3600. + 125.), "1:02:05");
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
