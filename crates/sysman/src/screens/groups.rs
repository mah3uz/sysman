use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use gpui::{
    AnyElement, App, AppContext, Context, Div, Entity, FontWeight, Hsla, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt, DropdownMenu, PopupMenuItem};
use gpui_component::tooltip::Tooltip;
use gpui_component::{Disableable, Icon, Sizable, WindowExt};
use gpui_kit_assets::IconName;
use schema::processes::Process;

use crate::charts::format::{Unit, bytes};
use crate::charts::sparkline::Sparkline;
use crate::charts::trend::{ChartState, TrendChart, YScale};
use crate::data::desktop::IconIndex;
use crate::data::groups::{
    self as model, Colour, Combine, Condition, Field, Group, Op, Totals, cgroup_pids, config_path, families,
    heaviest_names, rules_for, totals, unit_name,
};
use crate::data::history::{Bucket, History, ProcField, bucket_width, db_path};
use crate::data::live::Live;
use crate::data::process::ProcessFeed;
use crate::data::source::top_up;
use crate::screens::processes::{Icons, process_icon};
use crate::ui::shell::Tab;
use crate::ui::theme::{Theme, theme};
use crate::ui::widgets::hairline;
use crate::ui::widgets::panel::caption_label;
use crate::ui::widgets::range::{Range, range_picker, remember, remembered};

const BUCKETS: u32 = 120;
const RELOAD_S: f64 = 60.;
const LIST_WIDTH: f32 = 330.;
const CHART_HEIGHT: f32 = 190.;
/// The daemon records processes every 10 s.
const RECORD_INTERVAL_S: f64 = 10.;
const SUGGESTED_UNITS: usize = 5;
/// Command names a unit preset lists, so a busy session unit does not make a rule of fifty rows.
const PRESET_NAMES: usize = 8;
const EDITOR_WIDTH: f32 = 680.;
const PREVIEW_HEIGHT: f32 = 190.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Metric {
    Memory,
    Cpu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lens {
    Average,
    Peak,
}

fn colour(c: Colour, t: &Theme) -> Hsla {
    match c {
        Colour::Blue => t.blue,
        Colour::Green => t.green,
        Colour::Orange => t.orange,
        Colour::Red => t.red,
        Colour::Teal => t.teal,
        Colour::Purple => t.purple,
        Colour::Pink => t.pink,
        Colour::Indigo => t.indigo,
    }
}

/// The window's mean of bucket means, or its highest bucket peak.
fn reduce(data: &[Bucket], span: (f64, f64), width_s: f64, lens: Lens) -> Option<f64> {
    let visible = data
        .iter()
        .filter(|b| b.t as f64 + width_s >= span.0 && b.t as f64 <= span.1);
    match lens {
        Lens::Average => {
            let (sum, n) = visible.fold((0., 0usize), |(s, n), b| (s + b.avg, n + 1));
            (n > 0).then(|| sum / n as f64)
        }
        Lens::Peak => visible.map(|b| b.max).reduce(f64::max),
    }
}

/// `name`, or `name 2`, `name 3`… so group names stay unique.
fn unique_name(name: &str, groups: &[Group], except: Option<usize>) -> String {
    let taken = |n: &str| {
        groups
            .iter()
            .enumerate()
            .any(|(i, g)| Some(i) != except && g.name.eq_ignore_ascii_case(n))
    };
    let name = name.trim();
    if !taken(name) {
        return name.to_owned();
    }
    (2..)
        .map(|k| format!("{name} {k}"))
        .find(|n| !taken(n))
        .expect("unbounded")
}

fn wall_clock() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0., |d| d.as_secs_f64())
}

#[derive(Default)]
struct Series {
    rss: Vec<Bucket>,
    cpu: Vec<Bucket>,
}

/// History read for one range and one version of the group list.
struct Stored {
    range: Range,
    version: u64,
    series: Vec<Series>,
}

fn load(groups: Vec<Group>, range: Range, to: i64) -> Result<Vec<Series>> {
    let history = History::open(&db_path())?;
    let from = to - range.span_s();
    let procs = history.procs_between(from, to)?;
    groups
        .iter()
        .map(|g| {
            if !g.enabled {
                return Ok(Series::default());
            }
            let ids: Vec<i64> = procs
                .iter()
                .filter(|p| g.matches(&p.comm, p.cmdline.as_deref().unwrap_or(""), p.user.as_deref()))
                .map(|p| p.id)
                .collect();
            Ok(Series {
                rss: history.procs_sum(&ids, ProcField::Rss, from, to, BUCKETS)?,
                cpu: history.procs_sum(&ids, ProcField::Cpu, from, to, BUCKETS)?,
            })
        })
        .collect()
}

type Charted = Rc<[Bucket]>;

struct Suggestion {
    group: Group,
    source: &'static str,
    totals: Totals,
}

/// Presets for app families and the heaviest systemd units that are running now and not yet groups.
fn suggest(groups: &[Group], table: &[Process], units: Option<&schema::cgroups::Cgroups>) -> Vec<Suggestion> {
    let taken = |name: &str| groups.iter().any(|g| g.name.eq_ignore_ascii_case(name));
    let mut out: Vec<Suggestion> = families()
        .into_iter()
        .filter(|g| !taken(&g.name))
        .map(|group| Suggestion {
            totals: totals(&group, table),
            group,
            source: "App family",
        })
        .filter(|s| s.totals.count > 0)
        .collect();
    let Some(units) = units else {
        return out;
    };
    let mut seen: Vec<String> = Vec::new();
    for unit in units.top_memory.iter().chain(&units.top_cpu) {
        if seen.len() >= SUGGESTED_UNITS {
            break;
        }
        let name = unit_name(&unit.name);
        if name.is_empty() || taken(&name) || seen.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
            continue;
        }
        let rules = rules_for(&cgroup_pids(&unit.name), table, PRESET_NAMES);
        if rules.is_empty() {
            continue;
        }
        let tint = Colour::ALL[(seen.len() + 4) % Colour::ALL.len()];
        seen.push(name.clone());
        let group = Group::new(&name, tint, rules);
        out.push(Suggestion {
            totals: totals(&group, table),
            group,
            source: if unit.name.ends_with(".scope") {
                "App scope"
            } else {
                "Service"
            },
        });
    }
    out
}

pub struct Groups {
    live: Entity<Live>,
    feed: Entity<ProcessFeed>,
    path: PathBuf,
    groups: Vec<Group>,
    /// The file could not be read; saving is refused so it is never replaced.
    file_error: Option<SharedString>,
    save_error: Option<SharedString>,
    /// Bumped on every change to the list; history and the live trail follow it.
    version: u64,
    selected: Option<usize>,
    range: Range,
    metric: Metric,
    lens: Lens,
    stored: Option<Stored>,
    loading: bool,
    attempt: Option<(Range, u64, f64)>,
    history_error: Option<SharedString>,
    /// Live totals per group at each scan since the list last changed, to reach past the last recorded point.
    trail: Vec<(f64, Vec<Totals>)>,
    /// Memory and CPU per group: history topped up with the live trail.
    merged: Vec<(Charted, Charted)>,
    span: (f64, f64),
    width_s: i64,
    chart_version: u64,
    chart: Rc<RefCell<ChartState>>,
    suggestions: Vec<Suggestion>,
    icons: Arc<IconIndex>,
    detail_scroll: ScrollHandle,
    #[cfg(debug_assertions)]
    dev_editor: bool,
}

impl Groups {
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
        let path = config_path();
        let (groups, file_error) = match model::load(&path) {
            Ok(groups) => (groups, None),
            Err(e) => (
                Vec::new(),
                Some(format!("{e:#}. Fix or move the file; changes are not saved until then.").into()),
            ),
        };
        let mut this = Self {
            live,
            feed,
            path,
            selected: groups.iter().position(|g| g.enabled),
            groups,
            file_error,
            save_error: None,
            version: 0,
            range: remembered(Tab::Groups, cx),
            metric: Metric::Memory,
            lens: Lens::Average,
            stored: None,
            loading: false,
            attempt: None,
            history_error: None,
            trail: Vec::new(),
            merged: Vec::new(),
            span: (0., 0.),
            width_s: 1,
            chart_version: 0,
            chart: ChartState::new(),
            suggestions: Vec::new(),
            icons,
            detail_scroll: ScrollHandle::new(),
            #[cfg(debug_assertions)]
            dev_editor: std::env::var_os("SYSMAN_DEV_GROUP_EDITOR").is_some(),
        };
        #[cfg(debug_assertions)]
        if let Some(y) = std::env::var("SYSMAN_DEV_GROUP_SCROLL")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
        {
            this.detail_scroll.set_offset(gpui::point(px(0.), px(-y)));
        }
        this.on_feed(cx);
        this
    }

    fn table<'a>(&self, cx: &'a App) -> &'a [Process] {
        self.feed.read(cx).table().map_or(&[], |t| t.rows.as_slice())
    }

    fn now(&self, cx: &App) -> f64 {
        self.live.read(cx).latest.as_ref().map_or_else(wall_clock, |s| s.t)
    }

    fn on_feed(&mut self, cx: &mut Context<Self>) {
        let feed = self.feed.read(cx);
        let Some((t, table)) = &feed.latest else {
            self.refresh(cx);
            return;
        };
        let t = *t;
        let row = self.groups.iter().map(|g| totals(g, &table.rows)).collect();
        self.trail.retain(|(at, _)| *at >= t - self.range.span_s() as f64);
        self.trail.push((t, row));
        let units = self.live.read(cx).latest.as_ref().and_then(|s| s.cgroups.as_ref());
        self.suggestions = suggest(&self.groups, &table.rows, units);
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let now = self.now(cx);
        let stale = self
            .attempt
            .is_none_or(|(range, version, at)| range != self.range || version != self.version || now - at > RELOAD_S);
        if stale && !self.loading {
            self.load(now, cx);
        }
        self.merge(now);
        cx.notify();
    }

    fn load(&mut self, now: f64, cx: &mut Context<Self>) {
        self.loading = true;
        let (range, version, groups) = (self.range, self.version, self.groups.clone());
        self.attempt = Some((range, version, now));
        let task = cx
            .background_executor()
            .spawn(async move { load(groups, range, now as i64) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(series) => {
                        this.history_error = None;
                        this.stored = Some(Stored { range, version, series });
                    }
                    Err(e) => this.history_error = Some(format!("History unavailable: {e}").into()),
                }
                this.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    fn merge(&mut self, now: f64) {
        let to = now as i64;
        let from = to - self.range.span_s();
        self.width_s = bucket_width(from, to, BUCKETS);
        let stored = self
            .stored
            .as_ref()
            .filter(|s| s.range == self.range && s.version == self.version);
        let start = self.trail.first().map_or(f64::INFINITY, |(t, _)| *t);
        self.merged = (0..self.groups.len())
            .map(|i| {
                let series = stored.and_then(|s| s.series.get(i));
                let points = |pick: fn(&Totals) -> f64| {
                    self.trail
                        .iter()
                        .filter_map(move |(t, row)| Some(((t * 1000.).round() as i64, pick(row.get(i)?))))
                };
                let rss = top_up(
                    series.map_or(&[][..], |s| &s.rss),
                    start,
                    points(|t| t.rss as f64),
                    from,
                    self.width_s,
                );
                let cpu = top_up(
                    series.map_or(&[][..], |s| &s.cpu),
                    start,
                    points(|t| t.cpu),
                    from,
                    self.width_s,
                );
                (rss.into(), cpu.into())
            })
            .collect();
        self.span = (from as f64, now);
        self.chart_version += 1;
    }

    fn set_range(&mut self, range: Range, cx: &mut Context<Self>) {
        if range != self.range {
            self.range = range;
            remember(Tab::Groups, range, cx);
            self.refresh(cx);
        }
    }

    /// Applies a change to the list, saves the file and starts the history and live trail afresh.
    fn change(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Vec<Group>, &mut Option<usize>)) {
        f(&mut self.groups, &mut self.selected);
        if self
            .selected
            .is_none_or(|i| !self.groups.get(i).is_some_and(|g| g.enabled))
        {
            self.selected = self.groups.iter().position(|g| g.enabled);
        }
        self.version += 1;
        self.trail.clear();
        self.save_error = match &self.file_error {
            Some(_) => Some("Not saved: the groups file could not be read.".into()),
            None => model::save(&self.path, &self.groups)
                .err()
                .map(|e| format!("Not saved: {e:#}").into()),
        };
        self.on_feed(cx);
    }

    fn add(&mut self, mut group: Group, cx: &mut Context<Self>) {
        group.name = unique_name(&group.name, &self.groups, None);
        self.change(cx, |groups, selected| {
            groups.push(group);
            *selected = Some(groups.len() - 1);
        });
    }

    fn update(&mut self, i: usize, group: Group, cx: &mut Context<Self>) {
        self.change(cx, |groups, selected| {
            if let Some(slot) = groups.get_mut(i) {
                *slot = group;
                *selected = Some(i);
            }
        });
    }

    fn set_enabled(&mut self, i: usize, on: bool, cx: &mut Context<Self>) {
        self.change(cx, |groups, selected| {
            if let Some(g) = groups.get_mut(i) {
                g.enabled = on;
                if on {
                    *selected = Some(i);
                }
            }
        });
    }

    fn delete(&mut self, i: usize, cx: &mut Context<Self>) {
        self.change(cx, |groups, selected| {
            if i < groups.len() {
                groups.remove(i);
                *selected = selected.and_then(|s| match s.cmp(&i) {
                    std::cmp::Ordering::Less => Some(s),
                    std::cmp::Ordering::Equal => None,
                    std::cmp::Ordering::Greater => Some(s - 1),
                });
            }
        });
    }

    fn open_editor(&mut self, target: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let group = target
            .and_then(|i| self.groups.get(i).cloned())
            .unwrap_or_else(|| Group::new("New group", Colour::Blue, vec![Condition::new(Field::Comm, Op::Is, "")]));
        let screen = cx.entity().downgrade();
        let (live, feed) = (self.live.clone(), self.feed.clone());
        let others: Vec<String> = self
            .groups
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != target)
            .map(|(_, g)| g.name.to_lowercase())
            .collect();
        let editor = cx.new(|cx| Editor::new(target, &group, others, live, screen, feed, window, cx));
        let title = if target.is_some() { "Edit group" } else { "New group" };
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title(title)
                .w(px(EDITOR_WIDTH))
                .margin_top(px(40.))
                .child(editor.clone())
        });
    }

    fn open_rename(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(group) = self.groups.get(i) else {
            return;
        };
        let others = self
            .groups
            .iter()
            .enumerate()
            .filter(|(k, _)| *k != i)
            .map(|(_, g)| g.name.to_lowercase())
            .collect();
        let screen = cx.entity().downgrade();
        let name = group.name.clone();
        let renamer = cx.new(|cx| Renamer::new(i, &name, others, screen, window, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.title("Rename group").w(px(380.)).child(renamer.clone())
        });
    }

    fn confirm_delete(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.groups.get(i).map(|g| g.name.clone()) else {
            return;
        };
        let screen = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let screen = screen.clone();
            alert
                .title(SharedString::from(format!("Delete “{name}”?")))
                .description("The group and its rules are removed from groups.toml. Recorded history is kept.")
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .show_cancel(true)
                .on_ok(move |_, _, cx| {
                    screen.update(cx, |this, cx| this.delete(i, cx)).ok();
                    true
                })
        });
    }

    fn total_ram(&self, cx: &App) -> Option<u64> {
        total_ram(&self.live, cx)
    }

    fn cpus(&self, cx: &App) -> Option<usize> {
        self.live
            .read(cx)
            .latest
            .as_ref()
            .and_then(|s| s.cpu.as_ref()?.core_pct.as_ref())
            .map(Vec::len)
            .filter(|&n| n > 0)
    }

    fn current(&self, i: usize, cx: &App) -> Option<Totals> {
        let group = self.groups.get(i)?;
        self.feed.read(cx).table().map(|t| totals(group, &t.rows))
    }
}

fn total_ram(live: &Entity<Live>, cx: &App) -> Option<u64> {
    live.read(cx)
        .latest
        .as_ref()
        .and_then(|s| s.memory.as_ref())
        .map(|m| m.total_bytes)
        .filter(|&b| b > 0)
}

/// The desktop icon index once it has loaded, else an empty one.
fn icon_index(cx: &App) -> Arc<IconIndex> {
    cx.try_global::<Icons>().map(|i| i.0.clone()).unwrap_or_default()
}

fn caption(text: impl Into<SharedString>, cx: &App) -> Div {
    let t = theme(cx);
    div()
        .text_size(t.small_caption)
        .text_color(t.text_secondary)
        .child(text.into())
}

fn dot(color: Hsla, size: f32) -> Div {
    div().flex_shrink_0().size(px(size)).rounded_full().bg(color)
}

fn share(part: f64, whole: Option<u64>) -> String {
    match whole {
        Some(whole) => format!("{:.1}%", part / whole as f64 * 100.),
        None => "–".into(),
    }
}

/// A small segmented switch in the range picker's style.
fn segmented<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(T, &'static str)],
    current: T,
    on_select: impl Fn(T, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let t = theme(cx);
    let on_select = Rc::new(on_select);
    div()
        .flex()
        .flex_row()
        .p(px(2.))
        .rounded(px(6.))
        .bg(t.card)
        .text_size(t.caption)
        .children(options.iter().enumerate().map(|(k, &(value, label))| {
            let on_select = on_select.clone();
            let selected = value == current;
            div()
                .id((id, k))
                .h(px(20.))
                .px(px(8.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .when(selected, |d| d.bg(t.card_hover).text_color(t.text))
                .when(!selected, |d| {
                    d.text_color(t.text_secondary).hover(|d| d.text_color(t.text))
                })
                .child(label)
                .on_click(move |_, window, cx| on_select(value, window, cx))
        }))
}

fn frame(cx: &App) -> Div {
    let t = theme(cx);
    hairline(
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(t.panel_padding)
            .rounded(t.panel_radius)
            .bg(t.panel)
            .border_color(t.hairline),
        t.hairline_width,
    )
}

fn panel_title(icon: IconName, title: &str, cx: &App) -> Div {
    let t = theme(cx);
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
}

fn proportion(fraction: f64, color: Hsla, cx: &App) -> impl IntoElement {
    let t = theme(cx);
    div().h(px(5.)).w_full().rounded_full().bg(t.card_hover).child(
        div()
            .h_full()
            .rounded_full()
            .bg(color)
            .w(gpui::relative(fraction.clamp(0., 1.) as f32))
            .min_w(px(4.)),
    )
}

impl Groups {
    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let t = theme(cx);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .px(t.page_padding)
            .pt(px(14.))
            .pb(px(12.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).child("Groups"))
                            .child(
                                div()
                                    .id("groups-info")
                                    .child(Icon::new(IconName::Info).small().text_color(t.text_secondary))
                                    .tooltip(|window, cx| {
                                        Tooltip::new(
                                            "Share of RAM is the group's combined resident memory (RSS) over total \
                                             RAM. RSS counts shared pages in every process, so a group of related \
                                             processes can read higher than the memory it really holds.",
                                        )
                                        .build(window, cx)
                                    }),
                            ),
                    )
                    .child(caption(
                        "Sets of processes you define, with their combined memory and CPU and their share of this machine.",
                        cx,
                    )),
            )
            .child(div().flex_1())
            .when(self.loading, |d| d.child(caption("Loading…", cx)))
            .child(range_picker(
                self.range,
                move |range, _, cx| {
                    this.update(cx, |g, cx| g.set_range(range, cx)).ok();
                },
                cx,
            ))
            .child(
                Button::new("new-group")
                    .small()
                    .icon(Icon::new(IconName::Plus))
                    .label("New group")
                    .on_click(cx.listener(|this, _, window, cx| this.open_editor(None, window, cx))),
            )
    }

    fn group_card(&self, i: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let group = &self.groups[i];
        let t = theme(cx);
        let tint = colour(group.colour, t);
        let selected = self.selected == Some(i);
        let now = self.current(i, cx);
        let total = self.total_ram(cx);
        let names = heaviest_names(self.table(cx).iter().filter(|p| group.matches_process(p)), 3).join(", ");
        let detail = match now {
            Some(n) => format!(
                "{} {} · {}{}{}",
                n.count,
                if n.count == 1 { "process" } else { "processes" },
                bytes(n.rss as f64),
                if names.is_empty() { "" } else { " · " },
                names
            ),
            None => "Waiting for the process table…".into(),
        };
        let rss = self
            .merged
            .get(i)
            .map(|m| m.0.clone())
            .unwrap_or_else(|| Rc::from(Vec::new()));
        let gap = (self.width_s as f64 * 1.5).max(RECORD_INTERVAL_S * 2.5);
        let peak = reduce(&rss, self.span, self.width_s as f64, Lens::Peak);
        let mut spark = Sparkline::new(rss, tint, self.span, self.width_s as f64, gap);
        if let Some(peak) = peak {
            spark = spark.caption(format!("peak {}", bytes(peak)));
        }
        hairline(
            div()
                .id(("group", i))
                .flex()
                .flex_col()
                .gap(px(4.))
                .py(t.card_padding_y)
                .px(t.card_padding_x)
                .rounded(t.card_radius)
                .bg(if selected { t.card_hover } else { t.card })
                .border_color(if selected { tint.opacity(0.55) } else { t.hairline }),
            t.hairline_width,
        )
        .hover(move |s| s.bg(t.card_hover))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.selected = Some(i);
            cx.notify();
        }))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(7.))
                .child(dot(tint, 8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(group.name.clone()),
                )
                .child(
                    div()
                        .text_size(t.card_value)
                        .font_weight(FontWeight::SEMIBOLD)
                        .font_features(t.numbers.clone())
                        .child(now.map_or("–".into(), |n| share(n.rss as f64, total))),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(6.))
                .child(
                    caption(detail, cx)
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .font_features(t.numbers.clone()),
                )
                .child(caption("of RAM", cx)),
        )
        .child(spark)
        .context_menu(move |menu, _, _| {
            let (a, b, c, d) = (this.clone(), this.clone(), this.clone(), this.clone());
            menu.item(PopupMenuItem::new("Edit…").on_click(move |_, window, cx| {
                a.update(cx, |g, cx| g.open_editor(Some(i), window, cx)).ok();
            }))
            .item(PopupMenuItem::new("Rename…").on_click(move |_, window, cx| {
                b.update(cx, |g, cx| g.open_rename(i, window, cx)).ok();
            }))
            .item(PopupMenuItem::new("Disable").on_click(move |_, _, cx| {
                c.update(cx, |g, cx| g.set_enabled(i, false, cx)).ok();
            }))
            .separator()
            .item(PopupMenuItem::new("Delete…").on_click(move |_, window, cx| {
                d.update(cx, |g, cx| g.confirm_delete(i, window, cx)).ok();
            }))
        })
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled: Vec<usize> = (0..self.groups.len()).filter(|&i| self.groups[i].enabled).collect();
        let disabled: Vec<usize> = (0..self.groups.len()).filter(|&i| !self.groups[i].enabled).collect();
        let cards: Vec<AnyElement> = enabled
            .iter()
            .map(|&i| self.group_card(i, cx).into_any_element())
            .collect();
        let suggestions: Vec<AnyElement> = (0..self.suggestions.len())
            .map(|k| self.suggestion_row(k, cx).into_any_element())
            .collect();
        let disabled_rows: Vec<AnyElement> = disabled
            .iter()
            .map(|&i| self.disabled_row(i, cx).into_any_element())
            .collect();
        let t = theme(cx);
        let section = |label: &str, cx: &App| div().pt(px(10.)).pb(px(2.)).child(caption_label(label, cx));
        div()
            .id("groups-list")
            .flex_shrink_0()
            .w(px(LIST_WIDTH))
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(t.hairline)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .p(px(12.))
                    .when(cards.is_empty(), |d| {
                        d.child(
                            div()
                                .py(px(18.))
                                .px(px(6.))
                                .flex()
                                .flex_col()
                                .gap(px(4.))
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("No groups yet"),
                                )
                                .child(caption(
                                    "Create one with New group, or add a suggestion below with one click.",
                                    cx,
                                )),
                        )
                    })
                    .children(cards)
                    .when(!suggestions.is_empty(), |d| {
                        d.child(section("Suggested", cx)).children(suggestions)
                    })
                    .when(!disabled_rows.is_empty(), |d| {
                        d.child(section("Disabled", cx)).children(disabled_rows)
                    }),
            )
    }

    fn suggestion_row(&self, k: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let s = &self.suggestions[k];
        let t = theme(cx);
        let names = s
            .group
            .rules
            .iter()
            .map(|c| c.value.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let detail = format!(
            "{} · {} running · {}",
            s.source,
            s.totals.count,
            bytes(s.totals.rss as f64)
        );
        hairline(
            div()
                .id(("suggestion", k))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .py(px(7.))
                .px(px(10.))
                .rounded(t.sub_radius)
                .border_color(t.hairline),
            t.hairline_width,
        )
        .tooltip(move |window, cx| Tooltip::new(format!("Matches: {names}")).build(window, cx))
        .child(dot(colour(s.group.colour, t).opacity(0.6), 7.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(12.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(s.group.name.clone()),
                )
                .child(caption(detail, cx).font_features(t.numbers.clone())),
        )
        .child(
            Button::new(("add-suggestion", k))
                .ghost()
                .xsmall()
                .icon(Icon::new(IconName::Plus))
                .label("Add")
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(s) = this.suggestions.get(k) {
                        let group = s.group.clone();
                        this.add(group, cx);
                    }
                })),
        )
    }

    fn disabled_row(&self, i: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let group = &self.groups[i];
        let t = theme(cx);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .py(px(4.))
            .px(px(10.))
            .child(dot(colour(group.colour, t).opacity(0.4), 7.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.))
                    .text_color(t.text_secondary)
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(group.name.clone()),
            )
            .child(
                Button::new(("enable", i))
                    .ghost()
                    .xsmall()
                    .label("Enable")
                    .on_click(cx.listener(move |this, _, _, cx| this.set_enabled(i, true, cx))),
            )
            .child(
                Button::new(("delete-disabled", i))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::Trash))
                    .tooltip("Delete")
                    .on_click(cx.listener(move |this, _, window, cx| this.confirm_delete(i, window, cx))),
            )
    }

    fn detail(&self, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let toolbar = self.detail_toolbar(i, cx).into_any_element();
        let members = self.members(i, cx).into_any_element();
        let cx: &App = cx;
        let t = theme(cx);
        let group = &self.groups[i];
        let tint = colour(group.colour, t);
        let (rss, cpu) = self
            .merged
            .get(i)
            .cloned()
            .unwrap_or_else(|| (Rc::from(Vec::new()), Rc::from(Vec::new())));
        let width = self.width_s as f64;
        let memory = reduce(&rss, self.span, width, self.lens);
        let cpu_value = reduce(&cpu, self.span, width, self.lens);
        let total = self.total_ram(cx);
        let cpus = self.cpus(cx);
        let lens_word = match self.lens {
            Lens::Average => "average",
            Lens::Peak => "peak",
        };
        let over = format!("{lens_word} over {}", self.range.label());
        let (headline_label, headline, under) = match self.metric {
            Metric::Memory => (
                "Share of RAM",
                memory.map_or("–".into(), |m| share(m, total)),
                match (memory, total) {
                    (Some(m), Some(total)) => format!("{} of {} RAM · {over}", bytes(m), bytes(total as f64)),
                    _ => "No memory recorded in this window".into(),
                },
            ),
            Metric::Cpu => (
                "Share of CPU",
                match (cpu_value, cpus) {
                    (Some(c), Some(n)) => format!("{:.1}%", c / (n as f64 * 100.) * 100.),
                    _ => "–".into(),
                },
                match (cpu_value, cpus) {
                    (Some(c), Some(n)) => format!("{:.2} of {n} CPUs · {over}", c / 100.),
                    _ => "No CPU recorded in this window".into(),
                },
            ),
        };
        let running = self.current(i, cx).map(|n| n.count);
        let metric = |value: String, label: &str| {
            div()
                .flex()
                .flex_col()
                .items_end()
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .font_features(t.numbers.clone())
                        .child(value),
                )
                .child(caption(label.to_owned(), cx))
        };
        let summary = frame(cx)
            .flex_row()
            .items_start()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(5.))
                            .child(dot(tint, 6.))
                            .child(caption_label(headline_label, cx)),
                    )
                    .child(
                        div()
                            .text_size(px(34.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .font_features(t.numbers.clone())
                            .child(headline),
                    )
                    .child(caption(under, cx).font_features(t.numbers.clone())),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(metric(
                        memory.map_or("–".into(), bytes),
                        &format!("Memory ({lens_word})"),
                    ))
                    .child(metric(
                        cpu_value.map_or("–".into(), |c| format!("{:.2} cores", c / 100.)),
                        &format!("CPU ({lens_word})"),
                    ))
                    .child(metric(
                        running.map_or("–".into(), |n| n.to_string()),
                        "Processes running now",
                    )),
            );
        let gap = (width * 1.5).max(RECORD_INTERVAL_S * 2.5);
        let (data, unit, label) = match self.metric {
            Metric::Memory => (rss, Unit::Bytes, "Memory"),
            Metric::Cpu => (cpu, Unit::Percent, "CPU"),
        };
        let chart = TrendChart::new(self.chart.clone(), self.chart_version, self.span, width, gap)
            .line(data, tint, label)
            .unit(unit)
            .y(YScale::Zero)
            .fill()
            .height(px(CHART_HEIGHT));
        let (on_lens, on_metric) = (this.clone(), this);
        let note = match self.metric {
            Metric::Memory => {
                "Combined RSS of every matching process, summed at each scan, so a restart is not counted twice."
            }
            Metric::Cpu => "Combined CPU of every matching process, in percent of one core; can exceed 100%.",
        };
        let over_time = frame(cx)
            .child(
                panel_title(IconName::ChartLine, "Over time", cx)
                    .gap(px(8.))
                    .child(segmented(
                        "lens",
                        &[(Lens::Average, "Average"), (Lens::Peak, "Peak")],
                        self.lens,
                        move |lens, _, cx| {
                            on_lens
                                .update(cx, |g, cx| {
                                    g.lens = lens;
                                    cx.notify();
                                })
                                .ok();
                        },
                        cx,
                    ))
                    .child(segmented(
                        "metric",
                        &[(Metric::Memory, "Memory"), (Metric::Cpu, "CPU")],
                        self.metric,
                        move |metric, _, cx| {
                            on_metric
                                .update(cx, |g, cx| {
                                    g.metric = metric;
                                    cx.notify();
                                })
                                .ok();
                        },
                        cx,
                    )),
            )
            .child(chart)
            .child(caption(note, cx));
        div()
            .flex()
            .flex_col()
            .gap(t.gap)
            .child(toolbar)
            .child(summary)
            .child(over_time)
            .child(members)
            .into_any_element()
    }

    fn detail_toolbar(&self, i: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let group = &self.groups[i];
        let t = theme(cx);
        let rules = group.rules.iter().filter(|c| !c.value.trim().is_empty()).count();
        let combine = match group.combine {
            Combine::Any => "any",
            Combine::All => "all",
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(dot(colour(group.colour, t), 10.))
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
                            .child(group.name.clone()),
                    )
                    .child(caption(
                        format!(
                            "Matches {combine} of {rules} {}",
                            if rules == 1 { "condition" } else { "conditions" }
                        ),
                        cx,
                    )),
            )
            .child(div().flex_1())
            .child(
                Button::new("edit-group")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::Pencil))
                    .label("Edit")
                    .on_click(cx.listener(move |this, _, window, cx| this.open_editor(Some(i), window, cx))),
            )
            .child(
                Button::new("rename-group")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::PencilLine))
                    .label("Rename")
                    .on_click(cx.listener(move |this, _, window, cx| this.open_rename(i, window, cx))),
            )
            .child(
                Button::new("disable-group")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::EyeOff))
                    .label("Disable")
                    .on_click(cx.listener(move |this, _, _, cx| this.set_enabled(i, false, cx))),
            )
            .child(
                Button::new("delete-group")
                    .ghost()
                    .small()
                    .icon(Icon::new(IconName::Trash))
                    .label("Delete")
                    .on_click(cx.listener(move |this, _, window, cx| this.confirm_delete(i, window, cx))),
            )
    }

    fn members(&self, i: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let cx: &App = cx;
        let t = theme(cx);
        let group = &self.groups[i];
        let tint = colour(group.colour, t);
        let mut rows: Vec<&Process> = self.table(cx).iter().filter(|p| group.matches_process(p)).collect();
        rows.sort_by(|a, b| b.rss_bytes.cmp(&a.rss_bytes).then(a.pid.cmp(&b.pid)));
        let sum: u64 = rows.iter().map(|p| p.rss_bytes).sum();
        let count = rows.len();
        let cols: [(&str, f32); 6] = [
            ("Memory (RSS)", 100.),
            ("CPU", 64.),
            ("Threads", 64.),
            ("User", 96.),
            ("PID", 70.),
            ("Share", 120.),
        ];
        let cell = |w: f32, right: bool| {
            div()
                .w(px(w))
                .flex_shrink_0()
                .flex()
                .when(right, |d| d.justify_end())
                .px(px(6.))
        };
        let head = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(24.))
            .px(px(6.))
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .border_b_1()
            .border_color(t.hairline)
            .child(div().flex_1().min_w_0().child("Process"))
            .children(cols.iter().map(|&(label, w)| cell(w, label != "User").child(label)));
        let body = rows.iter().enumerate().map(|(k, p)| {
            let fraction = if sum > 0 { p.rss_bytes as f64 / sum as f64 } else { 0. };
            div()
                .flex()
                .flex_row()
                .items_center()
                .h(px(26.))
                .px(px(6.))
                .text_size(px(12.))
                .font_features(t.numbers.clone())
                .when(k % 2 == 1, |d| d.bg(gpui::white().opacity(0.03)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(process_icon(p, &self.icons, 16., cx))
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(p.comm.clone()),
                        ),
                )
                .child(cell(cols[0].1, true).child(bytes(p.rss_bytes as f64)))
                .child(cell(cols[1].1, true).child(format!("{:.1}%", p.cpu_pct)))
                .child(cell(cols[2].1, true).child(p.threads.to_string()))
                .child(
                    cell(cols[3].1, false)
                        .min_w_0()
                        .overflow_hidden()
                        .text_color(t.text_secondary)
                        .child(p.user.clone().unwrap_or_else(|| "–".into())),
                )
                .child(cell(cols[4].1, true).child(p.pid.to_string()))
                .child(
                    cell(cols[5].1, false)
                        .items_center()
                        .gap(px(6.))
                        .child(div().flex_1().child(proportion(fraction, tint, cx)))
                        .child(
                            div()
                                .w(px(34.))
                                .flex()
                                .justify_end()
                                .child(format!("{:.0}%", fraction * 100.)),
                        ),
                )
        });
        let empty = match self.feed.read(cx).table() {
            None => Some("Waiting for the process table…"),
            Some(_) if count == 0 => Some("No running process matches this group right now."),
            Some(_) => None,
        };
        frame(cx)
            .child(
                panel_title(IconName::List, "Members", cx).child(
                    caption(
                        format!("{count} running · {} · share of the group's memory", bytes(sum as f64)),
                        cx,
                    )
                    .font_features(t.numbers.clone()),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(head)
                    .children(body)
                    .children(empty.map(|e| caption(e, cx).py(px(14.)).flex().justify_center())),
            )
    }
}

impl Render for Groups {
    // `window` is only needed by the debug-build editor shortcut
    #[cfg_attr(not(debug_assertions), expect(unused_variables))]
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(debug_assertions)]
        if std::mem::take(&mut self.dev_editor) {
            let target = self.selected;
            cx.defer_in(window, move |this, window, cx| this.open_editor(target, window, cx));
        }
        let header = self.header(cx).into_any_element();
        let sidebar = self.sidebar(cx).into_any_element();
        let detail = match self.selected {
            Some(i) if i < self.groups.len() => self.detail(i, cx),
            _ => div()
                .pt(px(60.))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(6.))
                .child(Icon::new(IconName::Layers).size(px(28.)).text_color(theme(cx).text_tertiary))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(if self.groups.iter().any(|g| g.enabled) {
                            "No group selected"
                        } else {
                            "No groups yet"
                        }),
                )
                .child(caption(
                    "Groups add up the memory and CPU of processes that belong together, such as a browser and its helpers.",
                    cx,
                ))
                .into_any_element(),
        };
        let t = theme(cx);
        let notes: Vec<SharedString> = [&self.file_error, &self.save_error, &self.history_error]
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .children(notes.into_iter().map(|n| {
                div()
                    .px(t.page_padding)
                    .pb(px(6.))
                    .text_size(t.caption)
                    .text_color(t.orange)
                    .child(n)
            }))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .border_t_1()
                    .border_color(t.hairline)
                    .child(sidebar)
                    .child(
                        div()
                            .id("group-detail")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.detail_scroll)
                            .child(div().p(t.page_padding).child(detail)),
                    ),
            )
    }
}

struct Row {
    id: usize,
    field: Field,
    op: Op,
    not: bool,
    value: Entity<InputState>,
}

/// The group editor: name, colour and rules, with the processes the draft catches right now.
struct Editor {
    target: Option<usize>,
    /// Lower-cased names of the other groups.
    others: Vec<String>,
    live: Entity<Live>,
    screen: WeakEntity<Groups>,
    feed: Entity<ProcessFeed>,
    name: Entity<InputState>,
    colour: Colour,
    enabled: bool,
    combine: Combine,
    rows: Vec<Row>,
    next_id: usize,
}

impl Editor {
    #[allow(clippy::too_many_arguments)]
    fn new(
        target: Option<usize>,
        group: &Group,
        others: Vec<String>,
        live: Entity<Live>,
        screen: WeakEntity<Groups>,
        feed: Entity<ProcessFeed>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&feed, |_, _, cx| cx.notify()).detach();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Group name")
                .default_value(group.name.clone())
        });
        cx.subscribe(&name, |_, _, _: &InputEvent, cx| cx.notify()).detach();
        let mut this = Self {
            target,
            others,
            live,
            screen,
            feed,
            name,
            colour: group.colour,
            enabled: group.enabled,
            combine: group.combine,
            rows: Vec::new(),
            next_id: 0,
        };
        for c in &group.rules {
            this.add_row(c, window, cx);
        }
        this
    }

    fn add_row(&mut self, c: &Condition, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = c.field.placeholder();
        let value = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(c.value.clone())
        });
        cx.subscribe(&value, |_, _, _: &InputEvent, cx| cx.notify()).detach();
        self.rows.push(Row {
            id: self.next_id,
            field: c.field,
            op: c.op,
            not: c.not,
            value,
        });
        self.next_id += 1;
        cx.notify();
    }

    fn row(&mut self, id: usize) -> Option<&mut Row> {
        self.rows.iter_mut().find(|r| r.id == id)
    }

    fn draft(&self, cx: &App) -> Group {
        Group {
            name: self.name.read(cx).value().trim().to_owned(),
            colour: self.colour,
            enabled: self.enabled,
            combine: self.combine,
            rules: self
                .rows
                .iter()
                .map(|r| Condition {
                    field: r.field,
                    op: r.op,
                    value: r.value.read(cx).value().trim().to_owned(),
                    not: r.not,
                })
                .collect(),
        }
    }

    fn problem(&self, draft: &Group) -> Option<&'static str> {
        if draft.name.is_empty() {
            Some("Give the group a name.")
        } else if self.others.contains(&draft.name.to_lowercase()) {
            Some("Another group already has this name.")
        } else {
            None
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut draft = self.draft(cx);
        if self.problem(&draft).is_some() {
            return;
        }
        draft.rules.retain(|c| !c.value.is_empty());
        let target = self.target;
        self.screen
            .update(cx, |screen, cx| match target {
                Some(i) => screen.update(i, draft, cx),
                None => screen.add(draft, cx),
            })
            .ok();
        window.close_dialog(cx);
    }

    fn rule_row(&self, row: &Row, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = cx.entity().downgrade();
        let t = theme(cx);
        let id = row.id;
        let (field, op) = (row.field, row.op);
        let pick_field = editor.clone();
        let pick_op = editor;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .p(px(6.))
            .rounded(t.sub_radius)
            .bg(t.card)
            .child(
                div()
                    .id(("not", id))
                    .px(px(6.))
                    .py(px(3.))
                    .rounded(px(5.))
                    .text_size(t.small_caption)
                    .font_weight(FontWeight::BOLD)
                    .map(|d| {
                        if row.not {
                            d.bg(t.red.opacity(0.85)).text_color(gpui::white())
                        } else {
                            d.bg(t.card_hover).text_color(t.text_secondary)
                        }
                    })
                    .child("NOT")
                    .tooltip(|window, cx| Tooltip::new("Negate this condition").build(window, cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(r) = this.row(id) {
                            r.not = !r.not;
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new(("field", id))
                    .outline()
                    .small()
                    .w(px(130.))
                    .label(field.label())
                    .dropdown_menu_with_anchor(gpui::Anchor::TopLeft, move |mut menu, _, _| {
                        for f in Field::ALL {
                            let editor = pick_field.clone();
                            menu = menu.item(PopupMenuItem::new(f.label()).checked(f == field).on_click(
                                move |_, _, cx| {
                                    editor
                                        .update(cx, |e, cx| {
                                            if let Some(r) = e.row(id) {
                                                r.field = f;
                                            }
                                            cx.notify();
                                        })
                                        .ok();
                                },
                            ));
                        }
                        menu
                    }),
            )
            .child(
                Button::new(("op", id))
                    .outline()
                    .small()
                    .w(px(104.))
                    .label(op.label())
                    .dropdown_menu_with_anchor(gpui::Anchor::TopLeft, move |mut menu, _, _| {
                        for o in Op::ALL {
                            let editor = pick_op.clone();
                            menu = menu.item(PopupMenuItem::new(o.label()).checked(o == op).on_click(
                                move |_, _, cx| {
                                    editor
                                        .update(cx, |e, cx| {
                                            if let Some(r) = e.row(id) {
                                                r.op = o;
                                            }
                                            cx.notify();
                                        })
                                        .ok();
                                },
                            ));
                        }
                        menu
                    }),
            )
            .child(div().flex_1().min_w_0().child(Input::new(&row.value).small()))
            .child(
                Button::new(("remove", id))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::X))
                    .tooltip("Remove condition")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.rows.retain(|r| r.id != id);
                        cx.notify();
                    })),
            )
    }

    fn preview(&self, draft: &Group, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let tint = colour(self.colour, t);
        let table = self.feed.read(cx).table().map_or(&[][..], |t| t.rows.as_slice());
        let icons = icon_index(cx);
        let mut caught: Vec<&Process> = table.iter().filter(|p| draft.matches_process(p)).collect();
        caught.sort_by(|a, b| b.rss_bytes.cmp(&a.rss_bytes).then(a.pid.cmp(&b.pid)));
        let rss: u64 = caught.iter().map(|p| p.rss_bytes).sum();
        let cpu: f64 = caught.iter().map(|p| p.cpu_pct).sum();
        let heading = if !draft.has_conditions() {
            div()
                .flex()
                .flex_row()
                .gap(px(6.))
                .text_color(t.orange)
                .child("No conditions yet: this group matches nothing.")
        } else {
            div().flex().flex_row().gap(px(6.)).child(format!(
                "Caught now: {} {} · {} · {} of RAM · {:.1}% CPU",
                caught.len(),
                if caught.len() == 1 { "process" } else { "processes" },
                bytes(rss as f64),
                share(rss as f64, total_ram(&self.live, cx)),
                cpu
            ))
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .font_features(t.numbers.clone())
                    .child(dot(tint, 7.))
                    .child(heading),
            )
            .child(
                div()
                    .id("preview")
                    .h(px(PREVIEW_HEIGHT))
                    .overflow_y_scroll()
                    .rounded(t.sub_radius)
                    .bg(t.card)
                    .when(draft.has_conditions() && caught.is_empty(), |d| {
                        d.child(caption("Nothing running matches these rules right now.", cx).p(px(10.)))
                    })
                    .children(caught.iter().enumerate().map(|(k, p)| {
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .h(px(24.))
                            .px(px(8.))
                            .text_size(px(12.))
                            .font_features(t.numbers.clone())
                            .when(k % 2 == 1, |d| d.bg(gpui::white().opacity(0.03)))
                            .child(process_icon(p, &icons, 14., cx))
                            .child(
                                div()
                                    .w(px(130.))
                                    .flex_shrink_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(p.comm.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .text_color(t.text_tertiary)
                                    .font_family(t.mono.clone())
                                    .text_size(px(11.))
                                    .child(p.cmdline.clone()),
                            )
                            .child(div().w(px(76.)).flex().justify_end().child(bytes(p.rss_bytes as f64)))
                            .child(
                                div()
                                    .w(px(56.))
                                    .flex()
                                    .justify_end()
                                    .child(format!("{:.1}%", p.cpu_pct)),
                            )
                            .child(
                                div()
                                    .w(px(56.))
                                    .flex()
                                    .justify_end()
                                    .text_color(t.text_secondary)
                                    .child(p.pid.to_string()),
                            )
                    })),
            )
    }
}

impl Render for Editor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let draft = self.draft(cx);
        let problem = self.problem(&draft);
        let rows: Vec<AnyElement> = self
            .rows
            .iter()
            .map(|r| self.rule_row(r, cx).into_any_element())
            .collect();
        let editor = cx.entity().downgrade();
        let t = theme(cx);
        let swatches = div()
            .flex()
            .flex_row()
            .gap(px(6.))
            .children(Colour::ALL.iter().enumerate().map(|(k, &c)| {
                let selected = c == self.colour;
                div()
                    .id(("colour", k))
                    .size(px(20.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(selected, |d| d.border_2().border_color(t.text))
                    .child(dot(colour(c, t), 14.))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.colour = c;
                        cx.notify();
                    }))
            }));
        div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .text_size(px(13.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.))
                    .child(div().flex_1().child(Input::new(&self.name)))
                    .child(swatches),
            )
            .children(problem.map(|p| caption(p, cx).text_color(t.orange)))
            .child(caption_label("Membership", cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.))
                            .child(segmented(
                                "combine",
                                &[(Combine::Any, "ANY of"), (Combine::All, "ALL of")],
                                self.combine,
                                move |combine, _, cx| {
                                    editor
                                        .update(cx, |e, cx| {
                                            e.combine = combine;
                                            cx.notify();
                                        })
                                        .ok();
                                },
                                cx,
                            ))
                            .child(caption("the following conditions (case is ignored):", cx)),
                    )
                    .children(rows)
                    .child(
                        div().flex().flex_row().child(
                            Button::new("add-condition")
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(IconName::Plus))
                                .label("Condition")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.add_row(&Condition::new(Field::Comm, Op::Is, ""), window, cx);
                                })),
                        ),
                    ),
            )
            .child(div().h(px(1.)).bg(t.hairline))
            .child(self.preview(&draft, cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("cancel")
                            .small()
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("save")
                            .small()
                            .primary()
                            .label("Save")
                            .disabled(problem.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    ),
            )
    }
}

struct Renamer {
    index: usize,
    others: Vec<String>,
    screen: WeakEntity<Groups>,
    name: Entity<InputState>,
}

impl Renamer {
    fn new(
        index: usize,
        current: &str,
        others: Vec<String>,
        screen: WeakEntity<Groups>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Group name")
                .default_value(current.to_owned())
        });
        cx.subscribe(&name, |_, _, _: &InputEvent, cx| cx.notify()).detach();
        Self {
            index,
            others,
            screen,
            name,
        }
    }

    fn value(&self, cx: &App) -> String {
        self.name.read(cx).value().trim().to_owned()
    }

    fn problem(&self, cx: &App) -> Option<&'static str> {
        let name = self.value(cx);
        if name.is_empty() {
            Some("Give the group a name.")
        } else if self.others.contains(&name.to_lowercase()) {
            Some("Another group already has this name.")
        } else {
            None
        }
    }
}

impl Render for Renamer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let problem = self.problem(cx);
        let t = theme(cx);
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(Input::new(&self.name))
            .children(problem.map(|p| caption(p, cx).text_color(t.orange)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        Button::new("cancel-rename")
                            .small()
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("rename")
                            .small()
                            .primary()
                            .label("Rename")
                            .disabled(problem.is_some())
                            .on_click(cx.listener(|this, _, window, cx| {
                                let (i, name) = (this.index, this.value(cx));
                                this.screen
                                    .update(cx, |screen, cx| {
                                        if let Some(mut group) = screen.groups.get(i).cloned() {
                                            group.name = name;
                                            screen.update(i, group, cx);
                                        }
                                    })
                                    .ok();
                                window.close_dialog(cx);
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(t: i64, avg: f64, max: f64) -> Bucket {
        Bucket { t, min: avg, avg, max }
    }

    #[test]
    fn the_headline_reduces_only_the_visible_window() {
        let data = [b(0, 100., 900.), b(60, 10., 20.), b(120, 30., 40.)];
        assert_eq!(reduce(&data, (61., 180.), 60., Lens::Average), Some(20.));
        assert_eq!(
            reduce(&data, (61., 180.), 60., Lens::Peak),
            Some(40.),
            "a spike before the window does not set the peak"
        );
        assert_eq!(
            reduce(&[], (0., 60.), 60., Lens::Average),
            None,
            "no data is unknown, not zero"
        );
    }

    #[test]
    fn names_stay_unique_ignoring_case() {
        let groups = vec![
            Group::new("Browsers", Colour::Blue, vec![]),
            Group::new("browsers 2", Colour::Blue, vec![]),
        ];
        assert_eq!(unique_name("Chat", &groups, None), "Chat");
        assert_eq!(unique_name("BROWSERS", &groups, None), "BROWSERS 3");
        assert_eq!(
            unique_name("Browsers", &groups, Some(0)),
            "Browsers",
            "a group may keep its own name"
        );
    }
}
