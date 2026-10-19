mod findings;
mod inspector;
mod list;
mod model;
mod sources;
mod strip;
mod toolbar;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use gpui::{
    AppContext, Bounds, Context, Entity, FocusHandle, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, Pixels, Render, ScrollStrategy, SharedString, Styled, UniformListScrollHandle, Window, div,
};
use gpui_component::input::{InputEvent, InputState, TextareaState};
use jiff::tz::TimeZone;
use journal::Entry;
use journal::findings::{FindingKind, Severity};
use journal::packages::{Manager, Transaction};

use crate::data::logs::{
    self as data, Around, BootEnd, BootSummary, Bucket, CategoryCounts, Fetcher, Query, Scan, ScanState, Search,
    Status, now_us,
};
use crate::data::prefs::prefs;
use crate::ui::theme::theme;
use crate::ui::widgets::sidebar::{self, Side, Sidebar, Toggled, Widths};
use model::{Controls, FINDINGS_KEY, FindingsPref, Item, SAVED_KEY, Saved, SavedFilter, Span};

const SIDEBAR: Widths = Widths::new(224., 184., 340.);
const INSPECTOR: Widths = Widths::new(352., 280., 600.);
const BUCKETS: usize = 120;
const POLL: Duration = Duration::from_millis(200);
const DEBOUNCE: Duration = Duration::from_millis(250);
const AROUND: usize = 6;
const LINE_CACHE: usize = 4096;
const LINE_CHARS: usize = 400;

/// A finding as a card shows it, copied out of the index.
#[derive(Debug, Clone)]
struct Card {
    /// Its place in [`data::Index::findings`], which is what [`Query::finding`] takes.
    index: usize,
    kind: FindingKind,
    severity: Severity,
    title: String,
    evidence: String,
    count: usize,
}

/// What a computed [`View`] was computed from; a different key means it is out of date.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Key {
    scan: u64,
    query: u64,
    rows: usize,
    hits: usize,
    settled: bool,
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
    range: (u64, u64),
    boots: Vec<BootSummary>,
    cards: Vec<Card>,
    tallies: Vec<(FindingKind, usize)>,
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
}

#[cfg(debug_assertions)]
#[derive(Default)]
struct Dev {
    select: Option<String>,
    finding: Option<usize>,
    own_only: bool,
}

pub struct Logs {
    logs: data::Logs,
    scan: Arc<Scan>,
    search: Option<Arc<Search>>,
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
    lines: HashMap<u32, SharedString>,
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
        let (saved, findings) = {
            let p = prefs().lock().unwrap_or_else(PoisonError::into_inner);
            (
                p.get::<Saved>(SAVED_KEY).unwrap_or_default().filters,
                p.get::<FindingsPref>(FINDINGS_KEY).unwrap_or_default(),
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
            tz: TimeZone::system(),
            #[cfg(debug_assertions)]
            dev: Dev::default(),
        };
        #[cfg(debug_assertions)]
        this.dev_setup(cx);
        this.recompute(cx);
        this.watch(cx);
        this
    }

    /// Screenshot aids: `SYSMAN_DEV_LOGS_QUERY` (search text), `_RANGE` (`5min` … `7day`), `_BOOT` (`-1`, …),
    /// `_FINDING` (the nth card, once the scan ends), `_SELECT` (`error`, or rows back from the newest) and
    /// `_ACCESS=own` (the own-entries banner).
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
            let boots = journal::reader::Journal::open().and_then(|mut j| j.boots()).unwrap_or_default();
            if let Some(b) = boots.len().checked_sub(back + 1).and_then(|i| boots.get(i)) {
                self.set_span(if back == 0 { Span::ThisBoot } else { Span::Boot(b.id.clone()) }, cx);
            }
        }
        if let Some(text) = var("QUERY") {
            self.text = text;
            self.apply_text(cx);
        }
        self.dev.select = var("SELECT");
        self.dev.finding = var("FINDING").and_then(|f| f.parse().ok());
        self.dev.own_only = var("ACCESS").as_deref() == Some("own");
    }

    #[cfg(debug_assertions)]
    fn dev_apply(&mut self, cx: &mut Context<Self>) {
        if !self.view.key.settled {
            return;
        }
        if let Some(n) = self.dev.finding.take()
            && let Some(card) = self.view.cards.get(n)
        {
            self.set_finding(Some(card.index), cx);
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
                rows.iter().rev().copied().find(|&r| index.row(r).is_some_and(|r| r.priority <= 3))
            }
        };
        if let Some(row) = row {
            self.select(Some(row), cx);
            self.reveal(row);
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
            let text = self.inspected.as_ref().map(inspector::full_message).unwrap_or_default();
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
            self.search = self.logs.search(&self.query);
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

    fn set_finding(&mut self, finding: Option<usize>, cx: &mut Context<Self>) {
        self.set_controls(|c| c.finding = finding, cx);
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
        self.scan = self.logs.scan(scope);
        self.scan.set_follow(self.follow);
        self.scan_gen += 1;
        self.fetcher = Arc::default();
        self.lines.clear();
        self.wanted.clear();
        self.in_flight.clear();
        self.select(None, cx);
        // finding indexes and the strip's slice belong to the old index
        self.controls.finding = None;
        self.controls.window = None;
        self.query = self.controls.apply(self.parsed.clone());
        self.search = self.logs.search(&self.query);
        let boots = std::mem::take(&mut self.view.boots);
        self.view = View { boots, ..View::default() };
        self.stick = true;
        self.recompute(cx);
        self.watch(cx);
        cx.notify();
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        self.rescan(cx);
    }

    fn set_follow(&mut self, on: bool, cx: &mut Context<Self>) {
        self.follow = on;
        self.scan.set_follow(on);
        if on {
            self.stick = true;
            self.list.scroll_to_bottom();
        }
        self.watch(cx);
        cx.notify();
    }

    fn following(&self) -> bool {
        self.follow && self.span.reaches_now()
    }

    fn key(&self) -> Key {
        let state = self.scan.state();
        let searching = self.search.as_ref().is_some_and(|s| s.state().is_running());
        Key {
            scan: self.scan_gen,
            query: self.query_gen,
            rows: self.scan.index().len(),
            hits: self.search.as_ref().map_or(0, |s| s.hits().len()),
            settled: !state.is_running() && !searching,
        }
    }

    fn recompute(&mut self, cx: &mut Context<Self>) {
        if self.computing {
            return;
        }
        self.computing = true;
        let key = self.key();
        let (scan, search, query, span, from) = (
            self.scan.clone(),
            self.search.clone(),
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
                let requery = view.key.query != this.query_gen;
                this.apply_view(view, cx);
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
        if self.controls.finding.is_some_and(|f| !self.view.cards.iter().any(|c| c.index == f)) && self.view.scanned {
            self.set_finding(None, cx);
        }
        match self.selected {
            Some(row) if fresh_query => self.reveal(row),
            _ if self.stick || (at_end && self.following()) => self.list.scroll_to_bottom(),
            _ => {}
        }
        if !self.view.items.is_empty() {
            self.stick = false;
        }
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
        let key = self.key();
        let state = self.scan.state();
        if key != self.view.key {
            self.recompute(cx);
        }
        if state.is_running() || self.search.as_ref().is_some_and(|s| s.state().is_running()) {
            // the progress line moves even when no batch landed
            cx.notify();
        }
        let live = state.is_running() || state.is_following() || !key.settled || self.computing || key != self.view.key;
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
        let (scan, fetcher, gen) = (self.scan.clone(), self.fetcher.clone(), self.scan_gen);
        let task = cx
            .background_executor()
            .spawn(async move { read_entry(scan, &fetcher, row) });
        cx.spawn(async move |this, cx| {
            let inspected = task.await;
            this.update(cx, |this, cx| {
                if this.scan_gen == gen && this.selected == Some(row) {
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
    fn want_lines(&mut self, rows: Vec<u32>, cx: &mut Context<Self>) {
        self.wanted = rows
            .into_iter()
            .filter(|r| !self.lines.contains_key(r) && !self.in_flight.contains(r))
            .collect();
        self.fetch_lines(cx);
    }

    fn fetch_lines(&mut self, cx: &mut Context<Self>) {
        if !self.in_flight.is_empty() || self.wanted.is_empty() {
            return;
        }
        let rows = std::mem::take(&mut self.wanted);
        self.in_flight.extend(rows.iter().copied());
        let (scan, fetcher, gen) = (self.scan.clone(), self.fetcher.clone(), self.scan_gen);
        let task = cx
            .background_executor()
            .spawn(async move { read_lines(scan, &fetcher, &rows) });
        cx.spawn(async move |this, cx| {
            let lines = task.await;
            this.update(cx, |this, cx| {
                if this.scan_gen != gen {
                    return;
                }
                this.in_flight.clear();
                if this.lines.len() > LINE_CACHE {
                    this.lines.clear();
                }
                this.lines.extend(lines);
                this.fetch_lines(cx);
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
    let by_id: HashMap<&str, usize> = boots.iter().enumerate().map(|(i, b)| (b.id.as_str(), i)).collect();
    let items = model::items(
        &matches.rows,
        |r| index.row(r).and_then(|r| by_id.get(index.boot_id(r)).copied()),
        |b| !boots[b].current && boots[b].end != BootEnd::Running,
    );
    let findings = index.findings();
    let cards = model::ordered(findings)
        .into_iter()
        .map(|i| {
            let f = &findings[i];
            Card {
                index: i,
                kind: f.kind,
                severity: f.severity,
                title: f.title.clone(),
                evidence: f.evidence.clone(),
                count: f.count,
            }
        })
        .collect();
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
        range,
        tallies: model::tallies(findings),
        boots,
        cards,
        packages,
        scanned,
    }
}

fn with_fetcher<T>(scan: Arc<Scan>, fetcher: &Mutex<Option<Fetcher>>, f: impl FnOnce(&mut Fetcher) -> T) -> anyhow::Result<T> {
    let mut slot = fetcher.lock().unwrap_or_else(PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(Fetcher::new(scan)?);
    }
    Ok(f(slot.as_mut().expect("set above")))
}

fn read_lines(scan: Arc<Scan>, fetcher: &Mutex<Option<Fetcher>>, rows: &[u32]) -> Vec<(u32, SharedString)> {
    let packages: Vec<(u32, String)> = {
        let index = scan.index();
        rows.iter()
            .filter_map(|&r| index.row(r).and_then(|row| index.transaction(row)).map(|t| (r, model::transaction_line(t))))
            .collect()
    };
    let journal: Vec<u32> = rows.iter().copied().filter(|r| !packages.iter().any(|(p, _)| p == r)).collect();
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
        Err(e) => out.extend(journal.iter().map(|&r| (r, format!("(could not open the journal: {e})").into()))),
    }
    out
}

fn first_line(message: &str) -> String {
    let line = message.lines().next().unwrap_or("").trim_end();
    let mut out: String = line.chars().take(LINE_CHARS).collect();
    if out.len() < line.len() || message.contains('\n') {
        out.push('…');
    }
    out
}

fn read_entry(scan: Arc<Scan>, fetcher: &Mutex<Option<Fetcher>>, row: u32) -> Inspected {
    let transaction = {
        let index = scan.index();
        index.row(row).and_then(|r| index.transaction(r)).cloned()
    };
    let mut out = Inspected {
        row,
        entry: None,
        transaction,
        around: Around::default(),
        error: None,
    };
    if out.transaction.is_some() {
        return out;
    }
    let read = with_fetcher(scan, fetcher, |f| -> anyhow::Result<_> { Ok((f.entry(row)?, f.around(row, AROUND)?)) });
    match read.and_then(|r| r) {
        Ok((entry, around)) => {
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
        let toolbar = self.toolbar(cx).into_any_element();
        let banner = self.banner(cx);
        let sources = sidebar::showing(&self.left, cx).then(|| self.sources(cx).into_any_element());
        let sources = sidebar::sidebar(&self.left, sources, window, cx);
        let header = self.header(cx).into_any_element();
        let findings = self.findings(window, cx);
        let strip = self.strip(cx).into_any_element();
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
