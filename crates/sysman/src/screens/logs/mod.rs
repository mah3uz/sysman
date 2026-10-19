mod actions;
mod findings;
mod inspector;
mod lanes;
mod list;
mod model;
mod recorded;
mod sources;
mod strip;
mod toolbar;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use gpui::{
    AppContext, Bounds, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    MouseButton, ParentElement, Pixels, Render, ScrollStrategy, SharedString, Styled, UniformListScrollHandle, Window,
    div, prelude::FluentBuilder,
};
use gpui_component::input::{InputEvent, InputState, TextareaState};
use jiff::tz::TimeZone;
use journal::Entry;
use journal::findings::{FindingKind, Severity};
use journal::packages::{Manager, Transaction};

use crate::data::logs::{
    self as data, Around, BootEnd, BootSummary, Bucket, CategoryCounts, Fetcher, Lane, Query, Scan, ScanState, Search,
    Status, now_us,
};
use crate::data::prefs::prefs;
use crate::ui::shell::{Focus, take_focus};
use crate::ui::theme::theme;
use crate::ui::widgets::sidebar::{self, Side, Sidebar, Toggled, Widths};
use model::{Controls, FINDINGS_KEY, FindingsPref, Item, LANES_KEY, LanesPref, SAVED_KEY, Saved, SavedFilter, Span};
use recorded::{Ask, Recorded};

const SIDEBAR: Widths = Widths::new(224., 184., 340.);
const INSPECTOR: Widths = Widths::new(352., 280., 600.);
const BUCKETS: usize = 120;
const LANE_BUCKETS: usize = 240;
const POLL: Duration = Duration::from_millis(200);
const DEBOUNCE: Duration = Duration::from_millis(250);
const AROUND: usize = 6;
const LINE_CACHE: usize = 4096;
const LINE_CHARS: usize = 400;

/// One finding, or several folded together, as a card shows it, copied out of the index.
#[derive(Debug, Clone)]
struct Card {
    /// Places in [`data::Index::findings`], newest first, which is what [`Query::findings`] takes.
    members: Vec<usize>,
    kind: FindingKind,
    severity: Severity,
    title: String,
    evidence: String,
    count: usize,
    first_us: u64,
    last_us: u64,
    /// The process it is about, for a crash or OOM kill on its own.
    pid: Option<u32>,
}

/// What a computed [`View`] was computed from; a different key means it is out of date.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Key {
    scan: u64,
    query: u64,
    rows: usize,
    dropped: usize,
    hits: usize,
    settled: bool,
    lanes: bool,
}

/// The query's results over the index, computed on the background executor whenever the query changes or the
/// index grows, so a frame never walks the index.
#[derive(Default)]
struct View {
    key: Key,
    items: Arc<Vec<Item>>,
    entries: u64,
    counts: CategoryCounts,
    buckets: Vec<Bucket>,
    /// Filled only while the lanes view is open.
    lanes: Arc<Vec<Lane>>,
    range: (u64, u64),
    boots: Vec<BootSummary>,
    cards: Vec<Card>,
    /// Every finding as a card of its own, by its place in [`data::Index::findings`].
    singles: Vec<Card>,
    tallies: Vec<(FindingKind, usize, usize)>,
    packages: Option<&'static str>,
    /// Computed after the scan's first pass, so the findings and boot ends are in.
    scanned: bool,
}

struct Inspected {
    row: u32,
    entry: Option<Arc<Entry>>,
    transaction: Option<Transaction>,
    around: Around,
    error: Option<String>,
    /// The finding this entry is part of, by its place in [`data::Index::findings`].
    finding: Option<usize>,
    /// The process the entry is about: a crash's pid, or an OOM kill's victim.
    pid: Option<u32>,
}

#[cfg(debug_assertions)]
#[derive(Default)]
struct Dev {
    select: Option<String>,
    finding: Option<usize>,
    hover: Option<usize>,
    /// `backtrace` opens the selected crash's sheet; `restart` shows the restart confirmation, never running it;
    /// `nopolkit` shows what a restart without a polkit agent says.
    action: Option<String>,
}

/// A scan started while an older one's results are on screen; they stay until this one has enough to replace them.
struct Next {
    scan: Arc<Scan>,
    search: Option<Arc<Search>>,
    view: Option<View>,
}

/// A row to select once the scan reaches it, asked for by another screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// Waiting for the boot list to pick the scope.
    Scope(u64),
    Row(u64),
}

pub struct Logs {
    logs: data::Logs,
    scan: Arc<Scan>,
    search: Option<Arc<Search>>,
    next: Option<Next>,
    /// Until the first scan has been read, which is shown whole and at its newest entries.
    first: bool,
    /// The replaced scan's cards, kept until the scan on screen has its own.
    held: Option<Vec<Card>>,
    /// The latest scan started, which [`Logs::next`] holds until it is on screen.
    scan_gen: u64,
    query_gen: u64,
    span: Span,
    /// Where a [`Span::Last`] scope starts, fixed when its scan began.
    from_us: u64,
    text: String,
    parsed: Query,
    controls: Controls,
    query: Query,
    view: View,
    computing: bool,
    watching: bool,
    status: Option<Status>,
    fetcher: Arc<Mutex<Option<Fetcher>>>,
    /// First lines by their entry's time and hash, which outlive the row ids of any one scan, with when they came.
    lines: HashMap<(u64, u64), (SharedString, Instant)>,
    wanted: Vec<u32>,
    in_flight: HashSet<u32>,
    selected: Option<u32>,
    inspected: Option<Inspected>,
    /// The inspector folded away by its toggle while something is selected.
    folded: bool,
    follow: bool,
    left: Entity<Sidebar>,
    right: Entity<Sidebar>,
    findings_collapsed: bool,
    findings_all: bool,
    boots_all: bool,
    saved: Vec<SavedFilter>,
    naming: Option<Entity<InputState>>,
    input: Option<Entity<InputState>>,
    /// Empty the search field on the next frame, which has the window that takes.
    clear_input: bool,
    message: Option<Entity<TextareaState>>,
    message_row: Option<u32>,
    typed: u64,
    list: UniformListScrollHandle,
    /// Scroll to the newest row once the next results are in.
    stick: bool,
    keys: FocusHandle,
    strip: Rc<Cell<Bounds<Pixels>>>,
    drag: Option<(f32, f32)>,
    lanes_open: bool,
    /// The lane and bucket of the mark under the pointer.
    lane_hover: Option<(usize, usize)>,
    entry_rec: Option<Recorded>,
    finding_rec: Option<Recorded>,
    /// Readings before each boot end, by its time: `None` while read, `Some(None)` when history has nothing.
    readings: HashMap<u64, Option<Option<recorded::Last>>>,
    lanes_rec: Option<Recorded>,
    pending: Option<Pending>,
    /// The copy button showing its feedback, by row.
    copied: Option<(u32, &'static str)>,
    restart: Option<(u32, actions::Restart)>,
    /// A restart failed because no polkit agent could ask for the password; later confirmations say so up front.
    no_agent: bool,
    /// [`data::Index::dropped`] when the row ids in hand were taken.
    dropped: usize,
    tz: TimeZone,
    #[cfg(debug_assertions)]
    dev: Dev,
}

impl Logs {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let left = cx.new(|_| Sidebar::new("logs.left", Side::Left, SIDEBAR));
        let right = cx.new(|_| Sidebar::on_demand("logs.right", Side::Right, INSPECTOR));
        cx.subscribe(&right, |this: &mut Self, sidebar, _: &Toggled, cx| {
            // its own hide button folds it away just as the toolbar's toggle does
            let collapsed = sidebar.read(cx).collapsed();
            if collapsed == this.inspector_open() {
                this.folded = collapsed;
                cx.notify();
            }
        })
        .detach();
        let (saved, findings, lanes) = {
            let p = prefs().lock().unwrap_or_else(PoisonError::into_inner);
            (
                p.get::<Saved>(SAVED_KEY).unwrap_or_default().filters,
                p.get::<FindingsPref>(FINDINGS_KEY).unwrap_or_default(),
                p.get::<LanesPref>(LANES_KEY).unwrap_or_default(),
            )
        };
        let status = cx.background_executor().spawn(async { data::status() });
        cx.spawn(async move |this, cx| {
            let status = status.await;
            this.update(cx, |this, cx| {
                this.status = Some(status);
                cx.notify();
            })
            .ok();
        })
        .detach();
        let mut logs = data::Logs::default();
        let now = now_us();
        let span = Span::ThisBoot;
        let scan = logs.scan(span.scope(now));
        let mut this = Self {
            logs,
            scan,
            search: None,
            next: None,
            first: true,
            held: None,
            scan_gen: 0,
            query_gen: 0,
            span,
            from_us: now,
            text: String::new(),
            parsed: Query::default(),
            controls: Controls::default(),
            query: Query::default(),
            view: View::default(),
            computing: false,
            watching: false,
            status: None,
            fetcher: Arc::default(),
            lines: HashMap::new(),
            wanted: vec![],
            in_flight: HashSet::new(),
            selected: None,
            inspected: None,
            folded: false,
            follow: true,
            left,
            right,
            findings_collapsed: findings.collapsed,
            findings_all: false,
            boots_all: false,
            saved,
            naming: None,
            input: None,
            clear_input: false,
            message: None,
            message_row: None,
            typed: 0,
            list: UniformListScrollHandle::new(),
            stick: true,
            keys: cx.focus_handle(),
            strip: Rc::default(),
            drag: None,
            lanes_open: lanes.open,
            lane_hover: None,
            entry_rec: None,
            finding_rec: None,
            readings: HashMap::new(),
            lanes_rec: None,
            pending: None,
            copied: None,
            restart: None,
            no_agent: false,
            dropped: 0,
            tz: TimeZone::system(),
            #[cfg(debug_assertions)]
            dev: Dev::default(),
        };
        #[cfg(debug_assertions)]
        this.dev_setup(cx);
        if let Some(Focus::LogEntry { t_us }) = take_focus(cx) {
            this.open_at(t_us, cx);
        }
        this.recompute(cx);
        this.watch(cx);
        this
    }

    /// Screenshot aids: `SYSMAN_DEV_LOGS_QUERY` (search text), `_RANGE` (`5min` … `7day`), `_BOOT` (`-1`, …),
    /// `_FINDING` (the nth card, once the scan ends), `_SELECT` (`error`, or rows back from the newest),
    /// `_ACCESS=own` (read as if only the user's own journal were readable), `_COREDUMP=missing`, `_JOURNAL=none`,
    /// `_PACKAGES=dnf` (the "can't tell" notes), `_CAP` (a lower row cap), `_AT` (open at the entry this many seconds
    /// ago), `_HOVER` (hover the busiest mark of this lane), `_ACTION` (see [`Dev::action`]) and `_SWITCH`
    /// (`7day@6`: change to that range after so many seconds).
    #[cfg(debug_assertions)]
    fn dev_setup(&mut self, cx: &mut Context<Self>) {
        use crate::ui::widgets::range::Range;
        let var = |name: &str| std::env::var(format!("SYSMAN_DEV_LOGS_{name}")).ok();
        if let Some(r) = var("RANGE") {
            let range = Range::ALL.into_iter().find(|x| x.label().replace(' ', "") == r);
            if let Some(range) = range {
                self.set_span(Span::Last(range), cx);
            }
        }
        if let Some(back) = var("BOOT").and_then(|b| b.trim_start_matches('-').parse::<usize>().ok()) {
            let boots = journal::reader::Journal::open()
                .and_then(|mut j| j.boots())
                .unwrap_or_default();
            if let Some(b) = boots.len().checked_sub(back + 1).and_then(|i| boots.get(i)) {
                self.set_span(
                    if back == 0 {
                        Span::ThisBoot
                    } else {
                        Span::Boot(b.id.clone())
                    },
                    cx,
                );
            }
        }
        if let Some(text) = var("QUERY") {
            self.text = text;
            self.apply_text(cx);
        }
        self.dev.select = var("SELECT");
        self.dev.finding = var("FINDING").and_then(|f| f.parse().ok());
        self.dev.hover = var("HOVER").and_then(|l| l.parse().ok());
        self.dev.action = var("ACTION");
        if let Some(ago) = var("AT").and_then(|s| s.parse::<u64>().ok()) {
            self.open_at(now_us().saturating_sub(ago * 1_000_000), cx);
        }
        if let Some((r, after)) = var("SWITCH").as_deref().and_then(|s| s.split_once('@'))
            && let Some(range) = Range::ALL.into_iter().find(|x| x.label().replace(' ', "") == r)
            && let Ok(after) = after.parse::<f32>()
        {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Duration::from_secs_f32(after)).await;
                this.update(cx, |this, cx| this.set_span(Span::Last(range), cx)).ok();
            })
            .detach();
        }
    }

    #[cfg(debug_assertions)]
    fn dev_apply(&mut self, cx: &mut Context<Self>) {
        if !self.view.key.settled {
            return;
        }
        if let Some(li) = self.dev.hover
            && let Some(lane) = self.view.lanes.get(li)
        {
            let busiest = lane.counts.iter().enumerate().max_by_key(|(_, c)| **c).map(|(b, _)| b);
            self.lane_hover = busiest.map(|b| (li, b));
        }
        if let Some(n) = self.dev.finding.take()
            && let Some(card) = self.view.cards.get(n)
        {
            self.set_finding(Some(card.members.clone()), cx);
            return;
        }
        let Some(how) = self.dev.select.take() else { return };
        let rows: Vec<u32> = self
            .view
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Entry(v) => Some(v.row),
                Item::BootEnd(_) => None,
            })
            .collect();
        let row = match how.parse::<usize>() {
            Ok(back) => rows.len().checked_sub(back + 1).map(|i| rows[i]),
            Err(_) => {
                let index = self.scan.index();
                rows.iter()
                    .rev()
                    .copied()
                    .find(|&r| index.row(r).is_some_and(|r| r.priority <= 3))
            }
        };
        if let Some(row) = row {
            self.select(Some(row), cx);
            self.reveal(row);
        }
    }

    #[cfg(debug_assertions)]
    fn dev_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(i) = self.inspected.as_ref().filter(|i| Some(i.row) == self.selected) else {
            return;
        };
        match self.dev.action.take().as_deref() {
            Some("backtrace") => {
                if let Some(e) = i.entry.as_deref().filter(|e| actions::is_crash(e)) {
                    actions::open_backtrace(e, window, cx);
                }
            }
            Some("restart") => self.restart = Some((i.row, actions::Restart::Confirm)),
            Some("nopolkit") => {
                self.no_agent = true;
                self.restart = Some((i.row, actions::Restart::NoAgent));
            }
            _ => {}
        }
    }

    fn ensure_widgets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.input.is_none() {
            let text = self.text.clone();
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Search, or filter by unit: prio: boot: cat:")
                    .default_value(text)
            });
            cx.subscribe(&input, |this, input, event: &InputEvent, cx| match event {
                InputEvent::Change => {
                    this.text = input.read(cx).value().to_string();
                    this.debounce(cx);
                }
                InputEvent::PressEnter { .. } => {
                    this.text = input.read(cx).value().to_string();
                    this.apply_text(cx);
                }
                _ => {}
            })
            .detach();
            self.input = Some(input);
        }
        if std::mem::take(&mut self.clear_input)
            && let Some(input) = &self.input
        {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        if self.message.is_none() {
            self.message = Some(cx.new(|cx| {
                let mut s = TextareaState::new(window, cx).auto_grow(1, 40);
                s.set_readonly(true, cx);
                s
            }));
        }
        let shown = self
            .inspected
            .as_ref()
            .filter(|i| Some(i.row) == self.selected)
            .map(|i| i.row);
        if shown != self.message_row
            && let Some(message) = &self.message
        {
            self.message_row = shown;
            let text = self
                .inspected
                .as_ref()
                .map(inspector::shown_message)
                .unwrap_or_default();
            message.update(cx, |s, cx| s.set_value(text, window, cx));
        }
    }

    fn debounce(&mut self, cx: &mut Context<Self>) {
        self.typed += 1;
        let typed = self.typed;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            this.update(cx, |this, cx| {
                if this.typed == typed {
                    this.apply_text(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn apply_text(&mut self, cx: &mut Context<Self>) {
        self.typed += 1;
        let parsed = Query::parse(&self.text, now_us(), &self.tz);
        if parsed != self.parsed {
            self.parsed = parsed;
            self.refilter(cx);
        }
    }

    /// Recomposes the query from the text and the controls; a change in its free text or pids restarts the
    /// message search, anything else is answered from the index.
    fn refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.controls.apply(self.parsed.clone());
        if query == self.query {
            return;
        }
        let restart = query.search() != self.query.search();
        self.query = query;
        if restart {
            let search = self.logs.search(&self.query);
            match &mut self.next {
                Some(next) => next.search = search,
                None => self.search = search,
            }
        }
        self.query_gen += 1;
        self.stick = true;
        self.recompute(cx);
        self.watch(cx);
        cx.notify();
    }

    fn set_controls(&mut self, f: impl FnOnce(&mut Controls), cx: &mut Context<Self>) {
        f(&mut self.controls);
        self.refilter(cx);
    }

    fn set_finding(&mut self, finding: Option<Vec<usize>>, cx: &mut Context<Self>) {
        self.set_controls(|c| c.finding = finding, cx);
        match self
            .shown_card()
            .map(|c| Ask::over(secs(c.first_us), secs(c.last_us), c.pid))
        {
            Some(ask) => self.record(ask, |l| &mut l.finding_rec, cx),
            None => self.finding_rec = None,
        }
    }

    /// The card the list is narrowed to: a folded one, or a single finding picked from the inspector.
    fn shown_card(&self) -> Option<&Card> {
        let shown = self.controls.finding.as_ref()?;
        self.view
            .cards
            .iter()
            .find(|c| &c.members == shown)
            .or(match shown[..] {
                [one] => self.view.singles.get(one),
                _ => None,
            })
    }

    /// A boot end's evidence is what sysman last recorded before the cut, once read, as its last journal line
    /// rarely tells why.
    fn evidence(&self, card: &Card) -> String {
        if card.kind != FindingKind::BootWithoutShutdown {
            return card.evidence.clone();
        }
        match self.readings.get(&card.last_us) {
            Some(Some(Some(last))) => last.line(&self.tz),
            Some(Some(None)) => format!("Not recorded by sysman · {}", card.evidence),
            _ => card.evidence.clone(),
        }
    }

    fn read_readings(&mut self, cx: &mut Context<Self>) {
        let cuts: Vec<u64> = self
            .view
            .cards
            .iter()
            .filter(|c| c.kind == FindingKind::BootWithoutShutdown && !self.readings.contains_key(&c.last_us))
            .map(|c| c.last_us)
            .collect();
        for cut in cuts {
            self.readings.insert(cut, None);
            let next = self.view.boots.iter().map(|b| b.first_us).filter(|&f| f > cut).min();
            let until = next.map_or(secs(cut) + 300, secs);
            let task = cx
                .background_executor()
                .spawn(async move { recorded::last_before(secs(cut) - 1800, until) });
            cx.spawn(async move |this, cx| {
                let last = task.await;
                this.update(cx, |this, cx| {
                    this.readings.insert(cut, Some(last));
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }

    /// Picks the scope holding `t_us` and selects the row nearest it once the scan gets there.
    fn open_at(&mut self, t_us: u64, cx: &mut Context<Self>) {
        self.pending = Some(Pending::Scope(t_us));
        let boots = cx.background_executor().spawn(async {
            journal::reader::Journal::open()
                .and_then(|mut j| j.boots())
                .unwrap_or_default()
        });
        cx.spawn(async move |this, cx| {
            let boots = boots.await;
            this.update(cx, |this, cx| {
                if this.pending != Some(Pending::Scope(t_us)) {
                    return;
                }
                let at = boots.iter().position(|b| b.first_us <= t_us && t_us <= b.last_us);
                let span = match at {
                    Some(i) if i + 1 < boots.len() => Span::Boot(boots[i].id.clone()),
                    // the running boot, or a time no boot covers, which the running boot is the best guess for
                    _ => Span::ThisBoot,
                };
                this.pending = Some(Pending::Row(t_us));
                this.set_follow(false, cx);
                this.set_span(span, cx);
                this.settle_pending(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Selects the row nearest the asked-for time once the scan has read past it.
    fn settle_pending(&mut self, cx: &mut Context<Self>) {
        let Some(Pending::Row(t_us)) = self.pending else { return };
        let running = self.scan.state().is_running();
        if self.view.key.scan != self.scan_gen {
            return;
        }
        let nearest = {
            let index = self.scan.index();
            let reached = self
                .entry_rows()
                .last()
                .and_then(|(_, r)| index.row(r))
                .is_some_and(|r| r.t_us >= t_us);
            if running && !reached {
                return;
            }
            self.entry_rows()
                .filter_map(|(_, r)| index.row(r).map(|row| (r, row.t_us.abs_diff(t_us))))
                .min_by_key(|(_, d)| *d)
                .map(|(r, _)| r)
        };
        if let Some(row) = nearest {
            self.pending = None;
            self.select(Some(row), cx);
            self.reveal(row);
        } else if !running {
            self.pending = None;
        }
    }

    fn set_lanes_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.lanes_open = open;
        self.lane_hover = None;
        prefs()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set(LANES_KEY, &LanesPref { open });
        self.recompute(cx);
        cx.notify();
    }

    /// CPU and temperature behind the lanes, re-read only when the scope has moved by more than a sliver.
    fn record_lanes(&mut self, cx: &mut Context<Self>) {
        let (from, to) = (secs(self.view.range.0), secs(self.view.range.1));
        if to <= from {
            return;
        }
        let fresh = self.lanes_rec.as_ref().is_some_and(|r| {
            let (a, b) = r.ask.span;
            a == from && (b - to).abs() <= (to - from) / 100
        });
        if !fresh {
            let ask = Ask {
                span: (from, to),
                marks: vec![],
                pid: None,
                points: LANE_BUCKETS as u32,
            };
            self.record(ask, |l| &mut l.lanes_rec, cx);
        }
    }

    fn set_span(&mut self, span: Span, cx: &mut Context<Self>) {
        if span == self.span {
            return;
        }
        self.span = span;
        self.rescan(cx);
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        let now = now_us();
        let scope = self.span.scope(now);
        self.from_us = match scope.span {
            data::Span::Range { from_us, .. } => from_us,
            data::Span::Boot(_) => now,
        };
        let scan = self.logs.scan(scope);
        scan.set_follow(self.follow);
        self.scan_gen += 1;
        // finding indexes and the strip's slice belong to the old index
        self.controls.finding = None;
        self.finding_rec = None;
        self.lane_hover = None;
        self.controls.window = None;
        self.query = self.controls.apply(self.parsed.clone());
        let search = self.logs.search(&self.query);
        self.next = Some(Next {
            scan,
            search,
            view: None,
        });
        self.recompute(cx);
        self.watch(cx);
        cx.notify();
    }

    /// The scan being read, whether or not its results are on screen yet.
    fn reading(&self) -> &Arc<Scan> {
        self.next.as_ref().map_or(&self.scan, |n| &n.scan)
    }

    fn searching(&self) -> Option<&Arc<Search>> {
        match &self.next {
            Some(n) => n.search.as_ref(),
            None => self.search.as_ref(),
        }
        .filter(|s| s.state().is_running())
    }

    /// Whether what is on screen is the older scan's, waiting for [`Logs::next`].
    fn stale(&self) -> bool {
        self.next.is_some()
    }

    /// The newer scan's view replaces the older one once it is complete: the scan reads oldest first, so the
    /// newest entries the list opens at come last.
    fn ready(&self, view: &View) -> bool {
        view.key.settled || self.view.items.is_empty() || self.first
    }

    fn offer(&mut self, view: View, cx: &mut Context<Self>) {
        if self.ready(&view) {
            self.promote(view, cx);
        } else if let Some(next) = &mut self.next {
            next.view = Some(view);
            cx.notify();
        }
    }

    /// Puts the newer scan on screen, keeping the selected entry when it has it too and the older scan's cards
    /// until its own findings are in.
    fn promote(&mut self, view: View, cx: &mut Context<Self>) {
        let Some(next) = self.next.take() else { return };
        let carried = self.selected.and_then(|r| carry(&self.scan, &next.scan, r));
        match (self.selected, carried) {
            (Some(_), None) => self.select(None, cx),
            (Some(old), Some(new)) => {
                self.selected = Some(new);
                if let Some(i) = self.inspected.as_mut().filter(|i| i.row == old) {
                    i.row = new;
                    i.finding = view
                        .scanned
                        .then(|| {
                            let index = next.scan.index();
                            index
                                .findings()
                                .iter()
                                .position(|f| f.entries.contains(&(new as usize)))
                        })
                        .flatten();
                }
                if self.message_row == Some(old) {
                    self.message_row = Some(new);
                }
                if let Some((r, _)) = &mut self.restart {
                    *r = new;
                }
                self.copied = None;
            }
            _ => {}
        }
        self.held = (!view.scanned).then(|| std::mem::take(&mut self.view.cards));
        self.scan = next.scan;
        self.search = next.search;
        self.fetcher = Arc::default();
        self.dropped = view.key.dropped;
        self.wanted.clear();
        self.in_flight.clear();
        self.stick = carried.is_none();
        self.apply_view(view, cx);
        if let Some(row) = carried {
            self.reveal(row);
        }
    }

    fn forget_rows(&mut self, cx: &mut Context<Self>) {
        self.wanted.clear();
        self.in_flight.clear();
        self.select(None, cx);
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        self.rescan(cx);
    }

    fn set_follow(&mut self, on: bool, cx: &mut Context<Self>) {
        self.follow = on;
        self.scan.set_follow(on);
        if let Some(next) = &self.next {
            next.scan.set_follow(on);
        }
        if on {
            self.stick = true;
            self.list.scroll_to_bottom();
        }
        self.watch(cx);
        cx.notify();
    }

    fn following(&self) -> bool {
        self.follow && self.span.reaches_now() && !self.follow_capped()
    }

    fn own_only(&self) -> bool {
        self.status
            .is_some_and(|s| s.access == journal::reader::Access::OwnOnly)
    }

    fn follow_capped(&self) -> bool {
        self.scan.state() == ScanState::FollowCapped
    }

    /// The scan and search the next view is computed from.
    fn target(&self) -> (&Arc<Scan>, Option<&Arc<Search>>) {
        match &self.next {
            Some(n) => (&n.scan, n.search.as_ref()),
            None => (&self.scan, self.search.as_ref()),
        }
    }

    fn key(&self) -> Key {
        let (scan, search) = self.target();
        let searching = search.is_some_and(|s| s.state().is_running());
        Key {
            scan: self.scan_gen,
            query: self.query_gen,
            rows: scan.index().len(),
            dropped: scan.index().dropped(),
            hits: search.map_or(0, |s| s.hits().len()),
            settled: !scan.state().is_running() && !searching,
            lanes: self.lanes_open,
        }
    }

    /// The key of the latest view computed for [`Logs::target`].
    fn latest(&self) -> Key {
        match &self.next {
            Some(n) => n.view.as_ref().map_or_else(Key::default, |v| v.key),
            None => self.view.key,
        }
    }

    fn recompute(&mut self, cx: &mut Context<Self>) {
        if self.computing {
            return;
        }
        self.computing = true;
        let key = self.key();
        let (scan, search) = self.target();
        let (scan, search, query, span, from) = (
            scan.clone(),
            search.cloned(),
            self.query.clone(),
            self.span.clone(),
            self.from_us,
        );
        let task = cx
            .background_executor()
            .spawn(async move { compute(&scan, search.as_deref(), &query, &span, from, key) });
        cx.spawn(async move |this, cx| {
            let view = task.await;
            this.update(cx, |this, cx| {
                this.computing = false;
                if view.key.scan != this.scan_gen {
                    this.recompute(cx);
                    return;
                }
                let requery = view.key.query != this.query_gen || view.key.lanes != this.lanes_open;
                if this.stale() {
                    this.offer(view, cx);
                } else {
                    this.apply_view(view, cx);
                }
                // growth is picked up by the poll at its own pace; a new query should not wait for it
                if requery {
                    this.recompute(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn apply_view(&mut self, view: View, cx: &mut Context<Self>) {
        let at_end = self.list.is_scrolled_to_end().unwrap_or(true);
        let fresh_query = view.key.query != self.view.key.query;
        self.view = view;
        self.first &= !self.view.key.settled;
        if self.view.scanned {
            self.held = None;
            if self.controls.finding.is_some() && self.shown_card().is_none() {
                self.set_finding(None, cx);
            }
            self.read_readings(cx);
        }
        match self.selected {
            Some(row) if fresh_query => self.reveal(row),
            _ if self.stick || (at_end && self.following()) => self.list.scroll_to_bottom(),
            _ => {}
        }
        if self.lanes_open && !self.view.lanes.is_empty() {
            self.record_lanes(cx);
        }
        self.settle_pending(cx);
        #[cfg(debug_assertions)]
        self.dev_apply(cx);
        cx.notify();
    }

    /// Polls the scan and search while either runs or follows, recomputing as the index grows.
    fn watch(&mut self, cx: &mut Context<Self>) {
        if self.watching {
            return;
        }
        self.watching = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let go = this.update(cx, |this, cx| this.poll(cx)).unwrap_or(false);
                if !go {
                    break;
                }
            }
        })
        .detach();
    }

    fn poll(&mut self, cx: &mut Context<Self>) -> bool {
        let dropped = self.scan.index().dropped();
        if dropped != self.dropped {
            self.dropped = dropped;
            self.forget_rows(cx);
        }
        let key = self.key();
        let state = self.target().0.state();
        if key != self.latest() {
            self.recompute(cx);
        }
        if state.is_running() || self.searching().is_some() {
            // the progress line moves even when no batch landed
            cx.notify();
        }
        let live = state.is_running() || state.is_following() || !key.settled || self.computing || key != self.latest();
        if !live {
            self.watching = false;
        }
        live
    }

    fn select(&mut self, row: Option<u32>, cx: &mut Context<Self>) {
        if row == self.selected {
            return;
        }
        self.selected = row;
        self.inspected = None;
        self.restart = None;
        self.entry_rec = None;
        if row.is_some() {
            self.stick = false;
        }
        if let Some(row) = row {
            self.inspect(row, cx);
        }
        self.sync_inspector(cx);
        cx.notify();
    }

    fn inspector_open(&self) -> bool {
        self.selected.is_some() && !self.folded
    }

    fn sync_inspector(&mut self, cx: &mut Context<Self>) {
        let open = self.inspector_open();
        self.right.update(cx, |s, cx| s.set_collapsed(!open, cx));
    }

    fn toggle_inspector(&mut self, cx: &mut Context<Self>) {
        self.folded = !self.folded;
        self.sync_inspector(cx);
        cx.notify();
    }

    fn inspect(&mut self, row: u32, cx: &mut Context<Self>) {
        let (scan, fetcher) = (self.scan.clone(), self.fetcher.clone());
        let task = cx
            .background_executor()
            .spawn(async move { read_entry(scan, &fetcher, row) });
        let scan = self.scan.clone();
        cx.spawn(async move |this, cx| {
            let inspected = task.await;
            this.update(cx, |this, cx| {
                if Arc::ptr_eq(&this.scan, &scan) && this.selected == Some(row) {
                    let t = this.scan.index().row(row).map(|r| secs(r.t_us));
                    if let Some(t) = t.filter(|_| inspected.transaction.is_none()) {
                        this.record(Ask::around(t, inspected.pid), |l| &mut l.entry_rec, cx);
                    }
                    this.inspected = Some(inspected);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Asks for the messages of the rows on screen; only the latest ask is kept, so a fast scroll does not queue
    /// up every row it passed.
    fn want_lines(&mut self, rows: Vec<(u32, (u64, u64))>, cx: &mut Context<Self>) {
        self.wanted = rows
            .into_iter()
            .filter(|(r, key)| !self.lines.contains_key(key) && !self.in_flight.contains(r))
            .map(|(r, _)| r)
            .collect();
        self.fetch_lines(cx);
    }

    fn fetch_lines(&mut self, cx: &mut Context<Self>) {
        if !self.in_flight.is_empty() || self.wanted.is_empty() {
            return;
        }
        let rows = std::mem::take(&mut self.wanted);
        self.in_flight.extend(rows.iter().copied());
        let (scan, fetcher) = (self.scan.clone(), self.fetcher.clone());
        let task = cx
            .background_executor()
            .spawn(async move { read_lines(scan, &fetcher, &rows) });
        let scan = self.scan.clone();
        cx.spawn(async move |this, cx| {
            let lines = task.await;
            this.update(cx, |this, cx| {
                if this.lines.len() > LINE_CACHE {
                    this.lines.clear();
                }
                let now = Instant::now();
                this.lines
                    .extend(lines.into_iter().map(|(key, line)| (key, (line, now))));
                if Arc::ptr_eq(&this.scan, &scan) {
                    this.in_flight.clear();
                    this.fetch_lines(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn entry_rows(&self) -> impl Iterator<Item = (usize, u32)> + '_ {
        self.view.items.iter().enumerate().filter_map(|(i, item)| match item {
            Item::Entry(v) => Some((i, v.row)),
            Item::BootEnd(_) => None,
        })
    }

    fn reveal(&self, row: u32) {
        if let Some((i, _)) = self.entry_rows().find(|(_, r)| *r == row) {
            self.list.scroll_to_item(i, ScrollStrategy::Center);
        }
    }

    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let rows: Vec<(usize, u32)> = self.entry_rows().collect();
        if rows.is_empty() {
            return;
        }
        let at = self.selected.and_then(|s| rows.iter().position(|(_, r)| *r == s));
        let next = match at {
            Some(at) => (at as isize + by).clamp(0, rows.len() as isize - 1) as usize,
            None if by < 0 => rows.len() - 1,
            None => 0,
        };
        let (i, row) = rows[next];
        self.select(Some(row), cx);
        self.list.scroll_to_item(i, ScrollStrategy::Nearest);
    }

    fn set_findings_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        self.findings_collapsed = collapsed;
        prefs()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .set(FINDINGS_KEY, &FindingsPref { collapsed });
        cx.notify();
    }

    fn save_filters(&self) {
        prefs().lock().unwrap_or_else(PoisonError::into_inner).set(
            SAVED_KEY,
            &Saved {
                filters: self.saved.clone(),
            },
        );
    }

    fn use_filter(&mut self, query: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(input) = &self.input {
            input.update(cx, |s, cx| s.set_value(query.clone(), window, cx));
        }
        self.text = query;
        self.apply_text(cx);
    }

    fn typing(&self, window: &Window, cx: &gpui::App) -> bool {
        [&self.input, &self.naming]
            .into_iter()
            .flatten()
            .any(|i| i.read(cx).focus_handle(cx).contains_focused(window, cx))
    }
}

/// The query's results over the index. Hits are copied out first: holding the search's lock while waiting on
/// the index's could deadlock with the search thread, which takes them the other way round.
fn compute(scan: &Scan, search: Option<&Search>, q: &Query, span: &Span, from_us: u64, key: Key) -> View {
    let now = now_us();
    let scanned = !scan.state().is_running();
    let hits: Option<Vec<u32>> = search.map(|s| s.hits().clone());
    let hits = hits.as_deref();
    let index = scan.index();
    let matches = index.matching(q, hits);
    let counts = index.category_counts(q, hits);
    let boots = index.boot_summaries();
    let first = index.rows().first().map_or(now, |r| r.t_us);
    let range = match span {
        Span::Last(_) => (from_us, now),
        Span::ThisBoot => (boots.iter().find(|b| b.current).map_or(first, |b| b.first_us), now),
        Span::Boot(id) => boots
            .iter()
            .find(|b| &b.id == id)
            .map_or((first, now), |b| (b.first_us, b.last_us + 1)),
    };
    let buckets = index.histogram(q, hits, range.0, range.1.max(range.0 + 1), BUCKETS);
    let lanes = if key.lanes {
        Arc::new(index.lanes(q, hits, range.0, range.1.max(range.0 + 1), LANE_BUCKETS))
    } else {
        Arc::default()
    };
    let by_id: HashMap<&str, usize> = boots.iter().enumerate().map(|(i, b)| (b.id.as_str(), i)).collect();
    let items = model::items(
        &matches.rows,
        |r| index.row(r).and_then(|r| by_id.get(index.boot_id(r)).copied()),
        |b| !boots[b].current && boots[b].end != BootEnd::Running,
    );
    let findings = index.findings();
    let cards = model::grouped(findings)
        .into_iter()
        .map(|members| card(findings, members))
        .collect();
    let singles = (0..findings.len()).map(|i| card(findings, vec![i])).collect();
    let packages = index.transactions().first().map(|t| match t.manager {
        Manager::Pacman => "pacman.log",
        Manager::Dpkg => "dpkg.log",
        Manager::Apt => "apt history",
    });
    View {
        key,
        items: Arc::new(items),
        entries: matches.entries,
        counts,
        buckets,
        lanes,
        range,
        tallies: model::tallies(findings),
        boots,
        cards,
        singles,
        packages,
        scanned,
    }
}

/// `members` are newest first, as [`model::grouped`] gives them.
fn card(findings: &[journal::findings::Finding], members: Vec<usize>) -> Card {
    let f = &findings[members[0]];
    let same: Vec<usize> = members
        .iter()
        .copied()
        .filter(|&i| findings[i].kind == f.kind)
        .collect();
    let events = same.iter().map(|&i| findings[i].count).sum();
    let folded = same.len() > 1;
    Card {
        kind: f.kind,
        severity: f.severity,
        title: if folded {
            model::folded_title(f, same.len(), events)
        } else {
            f.title.clone()
        },
        evidence: match data::end_note(f) {
            Some(note) if f.kind == FindingKind::BootWithoutShutdown => format!("{} · {note}", f.evidence),
            _ => f.evidence.clone(),
        },
        count: events,
        first_us: members
            .iter()
            .map(|&i| findings[i].first_us)
            .min()
            .unwrap_or(f.first_us),
        last_us: f.last_us,
        pid: if folded { None } else { victim(f) },
        members,
    }
}

fn secs(us: u64) -> i64 {
    (us / 1_000_000) as i64
}

fn victim(f: &journal::findings::Finding) -> Option<u32> {
    matches!(f.kind, FindingKind::Crash | FindingKind::OomKill)
        .then(|| f.data.get("pid")?.parse().ok())
        .flatten()
}

fn with_fetcher<T>(
    scan: Arc<Scan>,
    fetcher: &Mutex<Option<Fetcher>>,
    f: impl FnOnce(&mut Fetcher) -> T,
) -> anyhow::Result<T> {
    let mut slot = fetcher.lock().unwrap_or_else(PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(Fetcher::new(scan)?);
    }
    Ok(f(slot.as_mut().expect("set above")))
}

/// The same entry in another scan's index, found by its time and hash.
fn carry(from: &Scan, to: &Scan, row: u32) -> Option<u32> {
    let (t_us, hash) = from.index().row(row).map(|r| (r.t_us, r.hash))?;
    let index = to.index();
    let at = index.rows().iter().position(|r| r.t_us == t_us && r.hash == hash)?;
    u32::try_from(at).ok()
}

pub(super) fn line_key(row: &data::Row) -> (u64, u64) {
    (row.t_us, row.hash)
}

fn read_lines(scan: Arc<Scan>, fetcher: &Mutex<Option<Fetcher>>, rows: &[u32]) -> Vec<((u64, u64), SharedString)> {
    let keys: HashMap<u32, (u64, u64)> = {
        let index = scan.index();
        rows.iter()
            .filter_map(|&r| Some((r, line_key(index.row(r)?))))
            .collect()
    };
    let packages: Vec<(u32, String)> = {
        let index = scan.index();
        rows.iter()
            .filter_map(|&r| {
                index
                    .row(r)
                    .and_then(|row| index.transaction(row))
                    .map(|t| (r, model::transaction_line(t)))
            })
            .collect()
    };
    let journal: Vec<u32> = rows
        .iter()
        .copied()
        .filter(|r| !packages.iter().any(|(p, _)| p == r))
        .collect();
    let mut out: Vec<(u32, SharedString)> = packages.into_iter().map(|(r, s)| (r, s.into())).collect();
    let read = with_fetcher(scan, fetcher, |f| {
        journal
            .iter()
            .map(|&r| {
                let line = match f.entry(r) {
                    Ok(Some(e)) => first_line(e.message()),
                    Ok(None) => "(no longer in the journal)".into(),
                    Err(e) => format!("(could not read: {e})"),
                };
                (r, SharedString::from(line))
            })
            .collect::<Vec<_>>()
    });
    match read {
        Ok(lines) => out.extend(lines),
        Err(e) => out.extend(
            journal
                .iter()
                .map(|&r| (r, format!("(could not open the journal: {e})").into())),
        ),
    }
    out.into_iter()
        .filter_map(|(r, line)| Some((*keys.get(&r)?, line)))
        .collect()
}

fn first_line(message: &str) -> String {
    let message = model::plain(message);
    let line = message.trim_start().lines().next().unwrap_or("").trim_end();
    let mut out: String = line.chars().take(LINE_CHARS).collect();
    if out.len() < line.len() || message.contains('\n') {
        out.push('…');
    }
    out
}

fn read_entry(scan: Arc<Scan>, fetcher: &Mutex<Option<Fetcher>>, row: u32) -> Inspected {
    let (transaction, finding, victim) = {
        let index = scan.index();
        let findings = index.findings();
        let finding = findings.iter().position(|f| f.entries.contains(&(row as usize)));
        (
            index.row(row).and_then(|r| index.transaction(r)).cloned(),
            finding,
            finding.and_then(|f| victim(&findings[f])),
        )
    };
    let mut out = Inspected {
        row,
        entry: None,
        transaction,
        around: Around::default(),
        error: None,
        finding,
        pid: victim,
    };
    if out.transaction.is_some() {
        return out;
    }
    let read = with_fetcher(scan, fetcher, |f| -> anyhow::Result<_> {
        Ok((f.entry(row)?, f.around(row, AROUND)?))
    });
    match read.and_then(|r| r) {
        Ok((entry, around)) => {
            let pid = entry.as_ref().and_then(|e| e.get("COREDUMP_PID")?.parse().ok());
            out.pid = pid.or(out.pid);
            out.entry = entry;
            out.around = around;
        }
        Err(e) => out.error = Some(format!("{e:#}")),
    }
    out
}

impl Render for Logs {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_widgets(window, cx);
        #[cfg(debug_assertions)]
        self.dev_action(window, cx);
        let toolbar = self.toolbar(cx).into_any_element();
        let banner = self.banner(cx);
        let sources = sidebar::showing(&self.left, cx).then(|| self.sources(cx).into_any_element());
        let sources = sidebar::sidebar(&self.left, sources, window, cx);
        let header = self.header(cx).into_any_element();
        let findings = self.findings(window, cx);
        let strip = if self.lanes_open {
            self.lanes(window, cx).into_any_element()
        } else {
            self.strip(cx).into_any_element()
        };
        let strip = div().flex_shrink_0().relative().child(strip).when(self.stale(), |d| {
            d.opacity(list::STALE)
                .child(div().id("logs-strip-stale").absolute().inset_0().occlude())
        });
        let group = self.finding_header(cx);
        let list = self.list(cx);
        let inspector = sidebar::showing(&self.right, cx).then(|| self.inspector(cx));
        let inspector = sidebar::sidebar(&self.right, inspector, window, cx);
        let status = self.status_bar(cx).into_any_element();
        let t = theme(cx);
        let keys = self.keys.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.window)
            .track_focus(&self.keys)
            .on_key_down(cx.listener(|this, e: &KeyDownEvent, window, cx| {
                if this.typing(window, cx) {
                    return;
                }
                match e.keystroke.key.as_str() {
                    "escape" if this.selected.is_some() => this.select(None, cx),
                    "up" => this.step(-1, cx),
                    "down" => this.step(1, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(toolbar)
            .children(banner)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(sources)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            // so the arrow keys and Escape reach this screen after a row is clicked
                            .on_mouse_down(MouseButton::Left, move |_, window, cx| keys.focus(window, cx))
                            .child(header)
                            .children(findings)
                            .child(strip)
                            .children(group)
                            .child(list),
                    )
                    .child(inspector),
            )
            .child(status)
    }
}

fn scan_failed(state: &ScanState) -> Option<&str> {
    match state {
        ScanState::Failed(e) => Some(e),
        _ => None,
    }
}
