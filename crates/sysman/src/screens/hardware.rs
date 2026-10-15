use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext, ClipboardItem, Context, Div, Entity, FontWeight, Hsla, InteractiveElement,
    IntoElement, ParentElement, PathBuilder, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled,
    Window, canvas, div, point, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::{Disableable, Icon, Sizable, WindowExt};
use gpui_kit_assets::IconName;
use schema::cpu::CpuMeta;
use schema::hwmon::HwmonMeta;
use schema::{Meta, Sample};

use crate::charts::format::Unit;
use crate::charts::trend::{ChartState, TrendChart, YScale};
use crate::data::hardware::{
    self, Category, Domain, Inventory, Node, Section, Sensor, clock_time, gib, installed_gib, product_title,
    threads_per_core,
};
use crate::data::history::{Bucket, History, bucket_width, db_path};
use crate::data::live::Live;
use crate::data::source::{Combine, Source};
use crate::screens::dashboard::Groups;
use crate::ui::theme::{Theme, theme};
use crate::ui::widgets::hairline;
use crate::ui::widgets::sidebar::{self, Side, Sidebar, Widths};

const OUTLINE: Widths = Widths::new(300., 240., 440.);
const CHART_SPAN_S: f64 = 300.;
const CHART_BUCKETS: u32 = 120;
const SEED_EVERY_S: f64 = 30.;
const SENSOR_CHART_HEIGHT: f32 = 96.;
const SENSOR_COLUMNS: usize = 4;
const LABEL_WIDTH: f32 = 200.;
const SENSOR_NOTE: &str = "Each chart is the hottest sensor of its domain over the last five minutes, live at the \
                           refresh cycle. Click a chart to watch every sensor behind it; Details lists each chip.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selection {
    Overview,
    Category(Category),
    Node(Category, usize),
}

fn category_icon(c: Category) -> IconName {
    match c {
        Category::System => IconName::PcCase,
        Category::Processor => IconName::Cpu,
        Category::Graphics => IconName::Gpu,
        Category::Memory => IconName::MemoryStick,
        Category::Displays => IconName::Monitor,
        Category::Storage => IconName::HardDrive,
        Category::Power => IconName::BatteryFull,
        Category::Sensors => IconName::Thermometer,
        Category::Network => IconName::Network,
        Category::Wifi => IconName::Wifi,
        Category::Bluetooth => IconName::Bluetooth,
        Category::Usb => IconName::Usb,
        Category::Audio => IconName::Volume2,
        Category::Pci => IconName::CircuitBoard,
        Category::Input => IconName::Keyboard,
        Category::Security => IconName::ShieldCheck,
        Category::Software => IconName::AppWindow,
    }
}

fn domain_icon(d: Domain) -> IconName {
    match d {
        Domain::Cpu => IconName::Cpu,
        Domain::Graphics => IconName::Gpu,
        Domain::Memory => IconName::MemoryStick,
        Domain::Storage => IconName::HardDrive,
        Domain::Board => IconName::CircuitBoard,
        Domain::Wireless => IconName::Wifi,
        Domain::Ethernet => IconName::EthernetPort,
        Domain::Usb => IconName::Usb,
        Domain::Other => IconName::Thermometer,
        Domain::Fans => IconName::Fan,
    }
}

/// Green through yellow to red as a sensor nears its limit.
fn heat_colour(heat: f64) -> Hsla {
    gpui::hsla((135. * (1. - heat) / 360.) as f32, 0.68, 0.56, 1.)
}

/// What the outline shows for a query: each section with its matching nodes, all of them when the section's own
/// title matches; sections with nothing to show are left out.
fn filter(sections: &[Section], query: &str) -> Vec<(usize, Vec<usize>)> {
    let needle = query.trim().to_lowercase();
    sections
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            if needle.is_empty() || s.category.title().to_lowercase().contains(&needle) {
                return Some((i, (0..s.nodes.len()).collect()));
            }
            let hits: Vec<usize> = (0..s.nodes.len()).filter(|&n| s.nodes[n].matches(&needle)).collect();
            (!hits.is_empty()).then_some((i, hits))
        })
        .collect()
}

fn node_text(category: Category, node: &Node) -> String {
    let mut out = format!("{} › {}\n", category.title(), node.title);
    for (l, v) in &node.props {
        out += &format!("{l}: {v}\n");
    }
    out
}

fn downloads_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Downloads")
}

pub struct Hardware {
    live: Entity<Live>,
    outline: Entity<Sidebar>,
    inventory: Option<Rc<Inventory>>,
    sections: Rc<Vec<Section>>,
    built_with_data: bool,
    scroll: ScrollHandle,
    #[cfg(debug_assertions)]
    dev_scroll: Option<f32>,
    #[cfg(debug_assertions)]
    dev_sensor_opened: bool,
    reading: bool,
    selected: Selection,
    expanded: HashSet<Category>,
    query: String,
    search: Option<Entity<InputState>>,
    seed: HashMap<Domain, Vec<Bucket>>,
    seeded_at: Option<f64>,
    seeding: bool,
    charts: RefCell<HashMap<Domain, Rc<RefCell<ChartState>>>>,
    version: u64,
}

impl Hardware {
    pub fn new(live: Entity<Live>, cx: &mut Context<Self>) -> Self {
        cx.observe(&live, |this, _, cx| {
            this.version += 1;
            let live = this.live.read(cx);
            if !this.built_with_data && live.meta.is_some() && live.latest.is_some() {
                this.rebuild(cx);
            }
            this.request_seed(cx);
            cx.notify();
        })
        .detach();
        #[cfg(debug_assertions)]
        let selected = std::env::var("SYSMAN_DEV_HW_SELECT")
            .ok()
            .and_then(|name| {
                Category::ALL
                    .into_iter()
                    .find(|c| c.title().eq_ignore_ascii_case(&name))
            })
            .map_or(Selection::Overview, Selection::Category);
        #[cfg(not(debug_assertions))]
        let selected = Selection::Overview;
        let mut this = Self {
            live,
            outline: cx.new(|_| Sidebar::new("hardware.left", Side::Left, OUTLINE)),
            inventory: None,
            sections: Rc::new(Vec::new()),
            built_with_data: false,
            scroll: ScrollHandle::new(),
            #[cfg(debug_assertions)]
            dev_scroll: None,
            #[cfg(debug_assertions)]
            dev_sensor_opened: false,
            reading: false,
            selected,
            expanded: HashSet::new(),
            query: String::new(),
            search: None,
            seed: HashMap::new(),
            seeded_at: None,
            seeding: false,
            charts: RefCell::default(),
            version: 0,
        };
        if let Selection::Category(c) = selected {
            this.expanded.insert(c);
        }
        #[cfg(debug_assertions)]
        {
            this.dev_scroll = std::env::var("SYSMAN_DEV_HW_SCROLL").ok().and_then(|y| y.parse().ok());
            this.query = std::env::var("SYSMAN_DEV_HW_QUERY").unwrap_or_default();
        }
        this.refresh(cx);
        this.request_seed(cx);
        this
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.reading {
            return;
        }
        self.reading = true;
        cx.notify();
        let task = cx
            .background_executor()
            .spawn(async move { hardware::read_inventory() });
        cx.spawn(async move |this, cx| {
            let inventory = task.await;
            this.update(cx, |this, cx| {
                this.reading = false;
                this.inventory = Some(Rc::new(inventory));
                this.rebuild(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn rebuild(&mut self, cx: &App) {
        let Some(inv) = &self.inventory else {
            return;
        };
        let live = self.live.read(cx);
        self.built_with_data = live.meta.is_some() && live.latest.is_some();
        self.sections = Rc::new(hardware::sections(inv, live.meta.as_ref(), live.latest.as_deref()));
    }

    /// Stored history fills the charts' first five minutes after a launch, before the live rings reach back.
    fn request_seed(&mut self, cx: &mut Context<Self>) {
        let live = self.live.read(cx);
        let now = live.latest.as_ref().map_or_else(hardware::now_s, |s| s.t);
        let sensors = hardware::sensors(live.latest.as_deref(), None);
        let covered = sensors
            .first()
            .and_then(|s| live.series(&s.path)?.front())
            .is_some_and(|&(t0, _)| t0 <= now - CHART_SPAN_S);
        // The daemon writes history in batches, so the seconds just before launch arrive late; re-read until the
        // live rings span the whole window.
        let due = self.seeded_at.is_none_or(|at| now - at >= SEED_EVERY_S);
        if self.seeding || sensors.is_empty() || covered || !due {
            return;
        }
        self.seeding = true;
        self.seeded_at = Some(now);
        let to = now as i64;
        let from = to - CHART_SPAN_S as i64;
        let mut sources: HashMap<Domain, Source> = HashMap::new();
        for s in sensors {
            sources
                .entry(s.domain)
                .or_insert_with(|| Source::new(Vec::new(), Combine::Max))
                .paths
                .push(s.path);
        }
        let task = cx.background_executor().spawn(async move {
            let history = History::open(&db_path())?;
            let mut out = HashMap::new();
            for (domain, source) in sources {
                out.insert(domain, source.query(&history, from, to, CHART_BUCKETS)?);
            }
            anyhow::Ok(out)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.seeding = false;
                if let Ok(seed) = result {
                    this.seed = seed;
                    this.version += 1;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn select(&mut self, selection: Selection, cx: &mut Context<Self>) {
        if selection != self.selected {
            self.scroll.set_offset(point(px(0.), px(0.)));
        }
        self.selected = selection;
        if let Selection::Category(c) | Selection::Node(c, _) = selection {
            self.expanded.insert(c);
        }
        cx.notify();
    }

    fn toggle(&mut self, c: Category, cx: &mut Context<Self>) {
        if !self.expanded.remove(&c) {
            self.expanded.insert(c);
        }
        cx.notify();
    }

    fn title(&self) -> String {
        self.inventory
            .as_ref()
            .and_then(|i| product_title(&i.dmi))
            .unwrap_or_else(|| "This computer".into())
    }

    fn export(&mut self, json: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(inv) = self.inventory.clone() else {
            return;
        };
        let title = self.title();
        let (text, ext) = if json {
            (hardware::json(&title, &self.sections, inv.at), "json")
        } else {
            (hardware::markdown(&title, &self.sections, inv.at), "md")
        };
        let name = format!("hardware-{}.{ext}", clock_time(inv.at as u64, "%Y%m%d-%H%M%S"));
        let dir = downloads_dir();
        let prompt = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |_, cx| {
            let path = match prompt.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                // No file chooser (no portal): save beside the user's other downloads instead.
                _ => dir.join(&name),
            };
            let result = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&path, text));
            let note = match result {
                Ok(()) => Notification::success(format!("Saved {}", path.display())),
                Err(e) => Notification::error(format!("Could not save {}: {e}", path.display())),
            };
            cx.update(|window, cx| window.push_notification(note, cx)).ok();
        })
        .detach();
    }

    fn ensure_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() {
            return;
        }
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search hardware"));
        cx.subscribe(&search, |this, search, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.query = search.read(cx).value().to_string();
                cx.notify();
            }
        })
        .detach();
        self.search = Some(search);
    }

    fn section(&self, c: Category) -> Option<&Section> {
        self.sections.iter().find(|s| s.category == c)
    }

    fn open_sensors(&mut self, domain: Domain, window: &mut Window, cx: &mut Context<Self>) {
        let live = self.live.clone();
        let sheet = cx.new(|cx| SensorSheet::new(live, domain, cx));
        window.open_dialog(cx, move |dialog, _, cx| {
            let t = theme(cx);
            dialog
                .w(px(480.))
                .title(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(7.))
                        .child(Icon::new(domain_icon(domain)).small().text_color(t.text_secondary))
                        .child(domain.title()),
                )
                .child(sheet.clone())
        });
    }
}

// ---------- header ----------

impl Hardware {
    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = theme(cx);
        let live = self.live.read(cx);
        let cpu: Option<CpuMeta> = module(live.meta.as_ref(), "cpu");
        let ram = live
            .latest
            .as_ref()
            .and_then(|s| s.memory.as_ref())
            .map(|m| m.total_bytes);
        let subtitle = [
            self.inventory.as_ref().and_then(|i| product_title(&i.dmi)),
            cpu.and_then(|c| c.model),
            ram.map(|r| format!("{} GB", installed_gib(r))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        let status = match (&self.inventory, self.reading) {
            (_, true) => div().child("Reading…").into_any_element(),
            (Some(inv), false) => div()
                .flex()
                .flex_col()
                .items_end()
                .child(format!("Updated {}", clock_time(inv.at as u64, "%H:%M:%S")))
                .child(
                    div()
                        .text_size(t.small_caption)
                        .text_color(t.text_tertiary)
                        .child(format!("{:.1} s", inv.took_s)),
                )
                .into_any_element(),
            (None, false) => div().into_any_element(),
        };
        let this = cx.entity().downgrade();
        let ready = self.inventory.is_some();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.))
            .px(t.page_padding)
            .py(px(12.))
            .bg(t.card.opacity(0.6))
            .border_b_1()
            .border_color(t.hairline)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(17.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Hardware"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(t.text_secondary)
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(if subtitle.is_empty() {
                                "Reading this computer…".into()
                            } else {
                                subtitle
                            }),
                    ),
            )
            .child(
                div()
                    .w(px(260.))
                    .flex_shrink_0()
                    .children(self.search.as_ref().map(|search| {
                        div().rounded(px(7.)).bg(t.card_hover).px(px(4.)).child(
                            Input::new(search)
                                .appearance(false)
                                .small()
                                .cleanable(true)
                                .prefix(Icon::new(IconName::Search).small().text_color(t.text_secondary)),
                        )
                    })),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(t.small_caption)
                    .text_color(t.text_secondary)
                    .font_features(t.numbers.clone())
                    .child(status),
            )
            .child(
                Button::new("hw-refresh")
                    .small()
                    .icon(Icon::new(IconName::RefreshCw))
                    .label("Refresh")
                    .disabled(self.reading)
                    .tooltip("Read the inventory again")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .child(
                Button::new("hw-export")
                    .small()
                    .ghost()
                    .icon(Icon::new(IconName::Share))
                    .label("Export")
                    .dropdown_caret(true)
                    .disabled(!ready)
                    .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, _| {
                        let (md, json) = (this.clone(), this.clone());
                        menu.item(PopupMenuItem::new("Markdown report…").on_click(move |_, window, cx| {
                            md.update(cx, |this, cx| this.export(false, window, cx)).ok();
                        }))
                        .item(PopupMenuItem::new("JSON…").on_click(move |_, window, cx| {
                            json.update(cx, |this, cx| this.export(true, window, cx)).ok();
                        }))
                    }),
            )
    }
}

// ---------- sidebar ----------

impl Hardware {
    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let t = theme(cx);
        let searching = !self.query.trim().is_empty();
        let row = |id: SharedString, selected: bool| {
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap(px(7.))
                .h(px(32.))
                .px(px(8.))
                .rounded(px(6.))
                .when(selected, |d| d.bg(t.selection))
                .when(!selected, |d| d.hover(|d| d.bg(t.card)))
        };
        let mut list = div().flex().flex_col().gap(px(1.)).child(
            row("hw-overview".into(), self.selected == Selection::Overview)
                .child(div().w(px(14.)))
                .child(Icon::new(IconName::LayoutDashboard).small().text_color(t.text))
                .child(div().text_size(px(13.)).child("Overview"))
                .on_click(cx.listener(|this, _, _, cx| this.select(Selection::Overview, cx))),
        );
        let visible = filter(&self.sections, &self.query);
        if searching && visible.is_empty() {
            list = list.child(
                div()
                    .px(px(8.))
                    .py(px(6.))
                    .text_size(t.caption)
                    .text_color(t.text_secondary)
                    .child("No matches"),
            );
        }
        for (si, nodes) in visible {
            let section = &self.sections[si];
            let c = section.category;
            let open = searching || self.expanded.contains(&c);
            let has_nodes = !section.nodes.is_empty();
            list = list.child(
                row(format!("hw-cat-{si}").into(), self.selected == Selection::Category(c))
                    .child(
                        div()
                            .id(("hw-chevron", si))
                            .w(px(14.))
                            .flex()
                            .justify_center()
                            .when(has_nodes, |d| {
                                d.child(
                                    Icon::new(if open {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .xsmall()
                                    .text_color(t.text_tertiary),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.toggle(c, cx);
                                }))
                            }),
                    )
                    .child(Icon::new(category_icon(c)).small().text_color(t.text))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(c.title()),
                    )
                    .when(has_nodes, |d| d.child(badge(section.nodes.len(), t)))
                    .on_click(cx.listener(move |this, _, _, cx| this.select(Selection::Category(c), cx))),
            );
            if open {
                for n in nodes {
                    let node = &section.nodes[n];
                    list = list.child(
                        row(
                            format!("hw-node-{si}-{n}").into(),
                            self.selected == Selection::Node(c, n),
                        )
                        .h(px(if node.subtitle.is_some() { 38. } else { 26. }))
                        .pl(px(52.))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .min_w_0()
                                .flex_1()
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(node.title.clone()),
                                )
                                .children(node.subtitle.clone().map(|s| {
                                    div()
                                        .text_size(t.small_caption)
                                        .text_color(t.text_secondary)
                                        .whitespace_nowrap()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(s)
                                })),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.select(Selection::Node(c, n), cx))),
                    );
                }
            }
        }
        list
    }
}

fn badge(n: usize, t: &Theme) -> impl IntoElement + use<> {
    div()
        .px(px(6.))
        .rounded_full()
        .bg(t.card_hover)
        .text_size(t.small_caption)
        .text_color(t.text_secondary)
        .font_features(t.numbers.clone())
        .child(n.to_string())
}

// ---------- detail ----------

impl Hardware {
    fn detail(&self, c: Category, only: Option<usize>, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        let Some(section) = self.section(c) else {
            let text = if self.reading {
                "Reading…"
            } else {
                "Nothing reported for this section."
            };
            return div().text_color(t.text_secondary).child(text).into_any_element();
        };
        let needle = self.query.trim().to_lowercase();
        let crumb = |label: String, sel: Selection, last: bool| {
            div()
                .id(SharedString::from(format!("crumb-{label}")))
                .text_color(if last { t.text } else { t.text_secondary })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.select(sel, cx)))
        };
        let mut crumbs = div()
            .flex()
            .flex_row()
            .gap(px(4.))
            .text_size(t.caption)
            .child(crumb("Hardware".into(), Selection::Overview, false))
            .child(div().text_color(t.text_tertiary).child("›"))
            .child(crumb(c.title().into(), Selection::Category(c), only.is_none()));
        let (title, subtitle, summary) = match only.and_then(|i| section.nodes.get(i)) {
            Some(node) => {
                crumbs = crumbs
                    .child(div().text_color(t.text_tertiary).child("›"))
                    .child(div().child(node.title.clone()));
                let count = node.props.len();
                (
                    node.title.clone(),
                    node.subtitle.clone(),
                    format!("{count} {}", if count == 1 { "property" } else { "properties" }),
                )
            }
            None => {
                let count = section.nodes.len();
                (
                    c.title().to_owned(),
                    None,
                    format!("{count} {}", if count == 1 { "item" } else { "items" }),
                )
            }
        };
        let copy_text: String = match only.and_then(|i| section.nodes.get(i)) {
            Some(node) => node_text(c, node),
            None => section.nodes.iter().map(|n| node_text(c, n) + "\n").collect(),
        };
        let heading = div()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(14.))
            .child(
                div()
                    .size(px(44.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(10.))
                    .bg(t.blue.opacity(0.12))
                    .child(Icon::new(category_icon(c)).size(px(26.)).text_color(t.blue)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .flex_1()
                    .min_w_0()
                    .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).child(title))
                    .children(subtitle.map(|s| div().text_size(px(12.)).text_color(t.text_secondary).child(s)))
                    .child(
                        div()
                            .text_size(t.small_caption)
                            .text_color(t.text_tertiary)
                            .child(summary),
                    ),
            )
            .child(
                Button::new("hw-copy")
                    .small()
                    .icon(Icon::new(IconName::Copy))
                    .label("Copy")
                    .tooltip("Copy as text")
                    .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy_text.clone()))),
            );
        let nodes: Vec<(usize, &Node)> = match only {
            Some(i) => section.nodes.get(i).map(|n| (i, n)).into_iter().collect(),
            None => {
                let shown = filter(std::slice::from_ref(section), &self.query);
                let keep = shown.first().map_or(&[][..], |(_, nodes)| nodes.as_slice());
                keep.iter().map(|&i| (i, &section.nodes[i])).collect()
            }
        };
        div()
            .flex()
            .flex_col()
            .gap(px(18.))
            .child(crumbs)
            .child(heading)
            .children(section.note.clone().filter(|_| only.is_none()).map(|note| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .p(px(10.))
                    .rounded(t.sub_radius)
                    .bg(gpui::hsla(0.14, 0.9, 0.5, 0.12))
                    .text_size(px(12.))
                    .text_color(t.text_secondary)
                    .child(Icon::new(IconName::Info).small())
                    .child(note)
            }))
            .when(nodes.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(12.))
                        .text_color(t.text_secondary)
                        .child(if needle.is_empty() {
                            "Nothing reported for this section."
                        } else {
                            "No matches in this section."
                        }),
                )
            })
            .children(nodes.into_iter().map(|(i, node)| {
                let props = props_table(&node.props, &needle, t);
                let mut panel = card_frame(t).gap(px(10.));
                if only.is_none() {
                    panel = panel.child(
                        div()
                            .id(("hw-node-title", i))
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .gap(px(8.))
                            .child(
                                div()
                                    .text_size(t.panel_title)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(node.title.clone()),
                            )
                            .children(
                                node.subtitle
                                    .clone()
                                    .map(|s| div().text_size(t.caption).text_color(t.text_secondary).child(s)),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| this.select(Selection::Node(c, i), cx))),
                    );
                }
                panel.child(props)
            }))
            .into_any_element()
    }
}

fn props_table(props: &[(String, String)], needle: &str, t: &Theme) -> impl IntoElement + use<> {
    if props.is_empty() {
        return div()
            .text_size(px(12.))
            .text_color(t.text_secondary)
            .child("No details reported.");
    }
    div()
        .flex()
        .flex_col()
        .gap(px(1.))
        .children(props.iter().map(|(label, value)| {
            let hit = hardware::prop_matches(label, value, needle);
            div()
                .flex()
                .flex_row()
                .gap(px(12.))
                .px(px(6.))
                .py(px(3.))
                .rounded(px(5.))
                .text_size(px(12.))
                .when(hit, |d| d.bg(gpui::hsla(0.14, 0.9, 0.5, 0.22)))
                .child(
                    div()
                        .w(px(LABEL_WIDTH))
                        .flex_shrink_0()
                        .flex()
                        .justify_end()
                        .text_color(t.text_secondary)
                        .child(label.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_features(t.numbers.clone())
                        .child(value.clone()),
                )
        }))
}

fn card_frame(t: &Theme) -> Div {
    hairline(
        div()
            .flex()
            .flex_col()
            .p(t.panel_padding)
            .rounded(t.panel_radius)
            .bg(t.panel)
            .border_color(t.hairline),
        t.hairline_width,
    )
}

// ---------- overview ----------

/// Label/value rows; rows without a value are left out.
fn facts(rows: Vec<(&str, Option<String>)>, label_width: f32, t: &Theme) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_col()
        .gap(px(5.))
        .text_size(px(12.))
        .children(rows.into_iter().filter_map(|(label, value)| {
            let value = value?;
            Some(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(12.))
                    .child(
                        div()
                            .w(px(label_width))
                            .flex_shrink_0()
                            .text_color(t.text_secondary)
                            .child(label.to_owned()),
                    )
                    .child(div().flex_1().min_w_0().font_features(t.numbers.clone()).child(value)),
            )
        }))
}

fn capacity_bar(fraction: f64, tint: Hsla, height: f32, t: &Theme) -> impl IntoElement + use<> {
    div().w_full().h(px(height)).rounded_full().bg(t.card_hover).child(
        div()
            .h_full()
            .rounded_full()
            .bg(tint.opacity(0.85))
            .w(gpui::relative(fraction.clamp(0., 1.) as f32)),
    )
}

fn pending(text: &str, t: &Theme) -> AnyElement {
    div()
        .text_size(px(12.))
        .text_color(t.text_secondary)
        .child(text.to_owned())
        .into_any_element()
}

fn module<T: serde::de::DeserializeOwned>(meta: Option<&Meta>, name: &str) -> Option<T> {
    serde_json::from_value(meta?.modules.get(name)?.info.clone()?).ok()
}

impl Hardware {
    /// A titled overview card whose "Details" link opens the category.
    fn card(&self, icon: IconName, title: &str, target: Category, cx: &mut Context<Self>) -> Div {
        let t = theme(cx);
        card_frame(t).flex_1().min_w_0().gap(px(14.)).child(
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
                .child(
                    div()
                        .id(SharedString::from(format!("details-{title}")))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(3.))
                        .text_size(t.small_caption)
                        .text_color(t.blue)
                        .hover(|d| d.opacity(0.75))
                        .child(Icon::new(IconName::ChevronRight).xsmall())
                        .child("Details")
                        .on_click(cx.listener(move |this, _, _, cx| this.select(Selection::Category(target), cx))),
                ),
        )
    }

    fn overview(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        let gap = t.gap;
        let pair = |a: Div, b: Div| div().flex().flex_row().gap(gap).child(a).child(b);
        let live = self.live.read(cx);
        let meta = live.meta.clone();
        let sample = live.latest.clone();
        let rows = [
            pair(
                self.computer_card(meta.as_ref(), sample.as_deref(), cx),
                self.processor_card(meta.as_ref(), cx),
            ),
            pair(
                self.memory_card(sample.as_deref(), cx),
                self.storage_card(meta.as_ref(), sample.as_deref(), cx),
            ),
            pair(self.displays_card(window, cx), self.power_card(cx)),
            pair(self.connectivity_card(cx), self.software_card(meta.as_ref(), cx)),
        ];
        div()
            .flex()
            .flex_col()
            .gap(gap)
            .children(rows)
            .child(self.sensors_card(meta.as_ref(), sample.as_deref(), cx))
            .into_any_element()
    }

    fn computer_card(&self, meta: Option<&Meta>, sample: Option<&Sample>, cx: &mut Context<Self>) -> Div {
        let card = self.card(IconName::PcCase, "This computer", Category::System, cx);
        let t = theme(cx);
        let Some(inv) = &self.inventory else {
            return card.child(pending("Reading…", t));
        };
        let dmi = |k: &str| inv.dmi.get(k).and_then(|v| hardware::meaningful(v)).map(str::to_owned);
        let cpu: Option<CpuMeta> = module(meta, "cpu");
        let gpus: Option<schema::gpu::Meta> = module(meta, "gpu");
        let board = dmi("board_name").map(|b| match dmi("board_vendor") {
            Some(v) => format!("{v} {b}"),
            None => b,
        });
        let firmware = dmi("bios_version").map(|v| {
            [dmi("bios_vendor"), Some(v), dmi("bios_date")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", ")
        });
        let graphics = gpus.map(|g| {
            g.devices
                .iter()
                .filter_map(|(slot, d)| hardware::gpu_name(d.name.as_deref(), inv.pci.iter().find(|p| &p.slot == slot)))
                .collect::<Vec<_>>()
                .join(", ")
        });
        card.child(
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(
                    div()
                        .text_size(t.card_value)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(self.title()),
                )
                .child(facts(
                    vec![
                        ("Motherboard", board),
                        ("Processor", cpu.and_then(|c| c.model)),
                        (
                            "Memory",
                            sample
                                .and_then(|s| s.memory.as_ref())
                                .map(|m| format!("{} GB", installed_gib(m.total_bytes))),
                        ),
                        ("Graphics", graphics.filter(|g| !g.is_empty())),
                        ("Firmware", firmware),
                        (
                            "Chassis",
                            dmi("chassis_type")
                                .and_then(|c| c.parse().ok())
                                .map(|c| hardware::chassis(c).to_owned()),
                        ),
                    ],
                    110.,
                    t,
                )),
        )
    }

    fn processor_card(&self, meta: Option<&Meta>, cx: &mut Context<Self>) -> Div {
        let card = self.card(IconName::Cpu, "Processor", Category::Processor, cx);
        let t = theme(cx);
        let Some(cpu) = module::<CpuMeta>(meta, "cpu") else {
            return card.child(pending("Waiting for the collector…", t));
        };
        let groups = Groups::new(Some(&cpu), t);
        let tpc = threads_per_core(&cpu);
        let block = |title: SharedString, colour: Hsla, cores: usize, subtitle: String, label: String| {
            let per_row = if cores > 10 {
                cores.div_ceil(cores.div_ceil(10))
            } else {
                cores.max(1)
            };
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(8.))
                .min_w(px(110.))
                .rounded(t.sub_radius)
                .bg(t.card)
                .border_1()
                .border_color(t.hairline)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(1.))
                        .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).child(title))
                        .child(
                            div()
                                .text_size(t.small_caption)
                                .text_color(t.text_secondary)
                                .child(subtitle),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .children((0..cores.div_ceil(per_row)).map(|r| {
                            div().flex().flex_row().gap(px(3.)).children(
                                (0..per_row.min(cores - r * per_row))
                                    .map(|_| div().size(px(12.)).rounded(px(3.)).bg(colour.opacity(0.85))),
                            )
                        })),
                )
                .child(
                    div()
                        .text_size(t.small_caption)
                        .text_color(t.text_secondary)
                        .child(label),
                )
        };
        let blocks: Vec<Div> = if groups.groups.is_empty() {
            let cores = cpu.topology.cores.unwrap_or(cpu.topology.threads) as usize;
            vec![block(
                "CPU".into(),
                t.blue,
                cores,
                format!("{cores} cores"),
                format!("{} threads", cpu.topology.threads),
            )]
        } else {
            groups
                .groups
                .iter()
                .enumerate()
                .map(|(i, (label, colour, cpus))| {
                    let cores = (cpus.len() as u32 / tpc) as usize;
                    let l3 = cpu
                        .topology
                        .l3_groups
                        .get(i)
                        .filter(|_| cpu.topology.core_types.is_none());
                    let subtitle = match l3 {
                        Some(g) => format!("{cores} cores, {} MB L3", g.l3_kib / 1024),
                        None => format!("{cores} cores"),
                    };
                    block(
                        label.clone(),
                        *colour,
                        cores,
                        subtitle,
                        format!("{} threads", cpus.len()),
                    )
                })
                .collect()
        };
        let freq = cpu
            .min_mhz
            .zip(cpu.max_mhz)
            .map(|(a, b)| format!("{:.2}–{:.2} GHz", a as f64 / 1000., b as f64 / 1000.));
        let strip_text = [
            cpu.topology.cores.map(|c| format!("{c} cores")),
            Some(format!("{} threads", cpu.topology.threads)),
            freq,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        let legend_item = |colour: Hsla, label: SharedString| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.))
                .child(div().size(px(9.)).rounded(px(2.)).bg(colour))
                .child(label)
        };
        card.child(
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(div().flex().flex_row().flex_wrap().gap(px(8.)).children(blocks))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(t.sub_radius)
                        .bg(t.blue.opacity(0.12))
                        .border_1()
                        .border_color(t.blue.opacity(0.35))
                        .text_size(px(12.))
                        .child(Icon::new(IconName::Zap).small().text_color(t.blue))
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .child(cpu.driver.clone().unwrap_or_else(|| "Frequency".into())),
                        )
                        .child(div().flex_1())
                        .child(div().text_color(t.text_secondary).child(strip_text)),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(12.))
                        .text_size(t.small_caption)
                        .text_color(t.text_secondary)
                        .children(
                            groups
                                .groups
                                .iter()
                                .map(|(label, colour, _)| legend_item(*colour, format!("{label} core").into())),
                        )
                        .children(
                            cpu.model
                                .clone()
                                .map(|m| div().font_weight(FontWeight::SEMIBOLD).child(m)),
                        ),
                ),
        )
    }

    fn memory_card(&self, sample: Option<&Sample>, cx: &mut Context<Self>) -> Div {
        let card = self.card(IconName::MemoryStick, "Memory", Category::Memory, cx);
        let t = theme(cx);
        let Some(m) = sample.and_then(|s| s.memory.as_ref()) else {
            return card.child(pending("Waiting for the collector…", t));
        };
        let used = m.total_bytes.saturating_sub(m.available_bytes);
        let dimms: Vec<f64> = hardware::sensors(sample, None)
            .into_iter()
            .filter(|s| s.domain == Domain::Memory)
            .map(|s| s.value)
            .collect();
        let range = (!dimms.is_empty()).then(|| {
            let lo = dimms.iter().copied().fold(f64::INFINITY, f64::min);
            let hi = dimms.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            if (hi - lo).abs() < 0.05 {
                format!("{hi:.1} °C")
            } else {
                format!("{lo:.1}–{hi:.1} °C")
            }
        });
        card.child(
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(
                    div()
                        .text_size(px(17.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("{} GB", installed_gib(m.total_bytes))),
                )
                .child(capacity_bar(
                    used as f64 / m.total_bytes.max(1) as f64,
                    t.purple,
                    10.,
                    t,
                ))
                .child(
                    div()
                        .text_size(t.small_caption)
                        .text_color(t.text_secondary)
                        .child(format!("{} in use of {} usable", gib(used), gib(m.total_bytes))),
                )
                .child(facts(
                    vec![
                        (
                            "Modules",
                            (!dimms.is_empty()).then(|| format!("{} with temperature sensors", dimms.len())),
                        ),
                        ("Temperature", range),
                        ("Swap", (m.swap_total_bytes > 0).then(|| gib(m.swap_total_bytes))),
                    ],
                    90.,
                    t,
                ))
                .child(
                    div()
                        .text_size(t.small_caption)
                        .text_color(t.text_tertiary)
                        .child("Module details need administrator access"),
                ),
        )
    }

    fn storage_card(&self, meta: Option<&Meta>, sample: Option<&Sample>, cx: &mut Context<Self>) -> Div {
        let card = self.card(IconName::HardDrive, "Storage", Category::Storage, cx);
        let t = theme(cx);
        let Some(fs) = sample.and_then(|s| s.fs.as_ref()).filter(|f| !f.is_empty()) else {
            let drives: Option<schema::storage::StorageMeta> = module(meta, "storage");
            let Some(drives) = drives.filter(|d| !d.devices.is_empty()) else {
                return card.child(pending("Waiting for the collector…", t));
            };
            return card.child(facts(
                drives
                    .devices
                    .iter()
                    .map(|(name, d)| (name.as_str(), Some(d.model.clone().unwrap_or_default())))
                    .collect(),
                90.,
                t,
            ));
        };
        let mut volumes: Vec<_> = fs.iter().collect();
        volumes.sort_by_key(|(mount, _)| (mount.as_str() != "/", mount.len()));
        card.child(
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .children(volumes.into_iter().take(5).map(|(mount, f)| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .text_size(px(12.))
                                .child(
                                    div()
                                        .flex_1()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(format!("{mount}  ({})", f.fstype)),
                                )
                                .child(
                                    div()
                                        .text_size(t.small_caption)
                                        .text_color(t.text_secondary)
                                        .child(gib(f.total_bytes)),
                                ),
                        )
                        .child(capacity_bar(
                            f.used_bytes as f64 / f.total_bytes.max(1) as f64,
                            t.blue,
                            8.,
                            t,
                        ))
                        .child(
                            div()
                                .text_size(t.small_caption)
                                .text_color(t.text_secondary)
                                .child(format!("{} free", gib(f.avail_bytes))),
                        )
                })),
        )
    }

    fn displays_card(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let card = self.card(IconName::Monitor, "Displays", Category::Displays, cx);
        let t = theme(cx);
        let Some(inv) = &self.inventory else {
            return card.child(pending("Reading…", t));
        };
        if inv.displays.is_empty() {
            return card.child(pending("No connected displays reported", t));
        }
        // Wayland names outputs after their connectors, and the window's display carries that name as a UUID.
        let here = window.display(cx).and_then(|d| d.uuid().ok());
        let is_main =
            |connector: &str| here == Some(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, connector.as_bytes()));
        let max_w = inv.displays.iter().map(|d| d.width).max().unwrap_or(1).max(1);
        card.child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(14.))
                .children(inv.displays.iter().map(|d| {
                    let aspect = if d.width > 0 {
                        d.height as f32 / d.width as f32
                    } else {
                        0.625
                    };
                    // Thumbnails are sized by resolution, so a 4K panel draws larger than a 1440p one.
                    let w = 90. + 50. * d.width as f32 / max_w as f32;
                    div()
                        .w(px(150.))
                        .flex()
                        .flex_col()
                        .gap(px(5.))
                        .child(
                            div()
                                .w(px(w))
                                .h(px(w * aspect))
                                .rounded(px(5.))
                                .bg(t.card_hover)
                                .border_2()
                                .border_color(t.text.opacity(0.3))
                                .flex()
                                .justify_end()
                                .p(px(4.))
                                .when(is_main(&d.connector), |b| {
                                    b.child(
                                        div()
                                            .h(px(14.))
                                            .px(px(5.))
                                            .rounded_full()
                                            .bg(t.blue.opacity(0.25))
                                            .text_size(t.small_caption)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child("Main"),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::MEDIUM)
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(d.name.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .text_size(t.small_caption)
                                .text_color(t.text_secondary)
                                .font_features(t.numbers.clone())
                                .when(d.width > 0, |c| c.child(format!("{} x {} pixels", d.width, d.height)))
                                .children(d.refresh.map(|r| {
                                    let size = d
                                        .size_mm
                                        .map(|(w, h)| format!(", {:.1}″", hardware::diagonal_in(w, h)))
                                        .unwrap_or_default();
                                    format!("{r:.2} Hz{size}")
                                })),
                        )
                        .child(
                            div()
                                .text_size(t.small_caption)
                                .text_color(t.text_tertiary)
                                .child(format!(
                                    "{}, {}",
                                    if d.builtin { "Built-in" } else { "External" },
                                    d.connector
                                )),
                        )
                })),
        )
    }

    fn power_card(&self, cx: &mut Context<Self>) -> Div {
        let battery = self
            .inventory
            .as_ref()
            .and_then(|i| i.supplies.iter().find(|s| s.is_system_battery()).cloned());
        let title = if battery.is_some() { "Battery" } else { "Power" };
        let card = self.card(IconName::BatteryFull, title, Category::Power, cx);
        let t = theme(cx);
        let Some(inv) = &self.inventory else {
            return card.child(pending("Reading…", t));
        };
        if let Some(b) = battery {
            let health = b.health_pct();
            let colour = match health {
                Some(h) if h >= 80. => t.green,
                Some(h) if h >= 60. => t.orange,
                _ => t.red,
            };
            let capacity = |full: &str, design: &str, unit: &str, scale: f64| {
                let f: f64 = b.get(full)?.parse().ok()?;
                let d: f64 = b.get(design)?.parse().ok()?;
                Some(format!("{:.0} of {:.0} {unit}", f / scale, d / scale))
            };
            return card.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(16.))
                    .child(ring(health.map(|h| h / 100.), colour, t))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .children(health.map(|h| {
                                div()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(format!("{h:.0}% of design capacity"))
                            }))
                            .child(facts(
                                vec![
                                    ("Status", b.get("STATUS").map(str::to_owned)),
                                    ("Cycle count", b.get("CYCLE_COUNT").map(str::to_owned)),
                                    ("Charge", b.get("CAPACITY").map(|c| format!("{c}%"))),
                                    (
                                        "Capacity",
                                        capacity("ENERGY_FULL", "ENERGY_FULL_DESIGN", "Wh", 1e6)
                                            .or_else(|| capacity("CHARGE_FULL", "CHARGE_FULL_DESIGN", "mAh", 1e3)),
                                    ),
                                ],
                                90.,
                                t,
                            )),
                    ),
            );
        }
        let devices: Vec<(String, Option<f64>, Option<String>)> = inv
            .supplies
            .iter()
            .filter(|s| s.get("SCOPE") == Some("Device"))
            .map(|s| {
                let name = match (s.get("MANUFACTURER"), s.get("MODEL_NAME")) {
                    (Some(m), Some(n)) => format!("{m} {n}"),
                    (m, n) => n.or(m).unwrap_or(&s.name).to_owned(),
                };
                (
                    name,
                    s.get("CAPACITY").and_then(|c| c.parse().ok()),
                    s.get("CAPACITY_LEVEL").map(str::to_owned),
                )
            })
            .collect();
        card.child(
            div()
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(t.text_secondary)
                        .child("No system battery: this computer runs on mains power."),
                )
                .when(!devices.is_empty(), |d| d.child(caption_upper("Device batteries", t)))
                .children(devices.into_iter().map(|(name, pct, level)| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .text_size(px(12.))
                                .child(div().flex_1().child(name))
                                .child(
                                    div()
                                        .text_color(t.text_secondary)
                                        .child(pct.map(|p| format!("{p:.0}%")).or(level).unwrap_or_else(|| "–".into())),
                                ),
                        )
                        .children(pct.map(|p| capacity_bar(p / 100., t.green, 7., t)))
                })),
        )
    }

    fn connectivity_card(&self, cx: &mut Context<Self>) -> Div {
        let card = self.card(IconName::Radio, "Connectivity", Category::Network, cx);
        let t = theme(cx);
        let Some(inv) = &self.inventory else {
            return card.child(pending("Reading…", t));
        };
        let join = |parts: Vec<Option<String>>| {
            let v: Vec<String> = parts.into_iter().flatten().collect();
            (!v.is_empty()).then(|| v.join(", "))
        };
        let wifi = inv.nets.iter().find(|n| n.wireless).and_then(|n| {
            join(vec![
                n.driver.clone(),
                n.state.clone(),
                n.signal_dbm.map(|s| format!("{s:.0} dBm")),
            ])
        });
        let ethernet = join(
            inv.nets
                .iter()
                .filter(|n| !n.wireless && n.state.as_deref() == Some("up"))
                .map(|n| {
                    Some(format!(
                        "{} {}",
                        n.name,
                        n.speed_mbps.map(|s| format!("{s} Mb/s")).unwrap_or_default()
                    ))
                })
                .collect(),
        );
        let bluetooth = inv.bluetooth.first().map(|b| {
            [
                b.chip.clone(),
                b.radio.clone(),
                Some(format!("{} connected", b.connections)),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ")
        });
        card.child(facts(
            vec![
                ("Wi-Fi", wifi.or_else(|| Some("No Wi-Fi interface".into()))),
                ("Ethernet", ethernet),
                ("Bluetooth", bluetooth.or_else(|| Some("No adapter".into()))),
                (
                    "USB devices",
                    Some(inv.usb.iter().filter(|u| !u.root).count().to_string()),
                ),
                ("PCI devices", Some(inv.pci.len().to_string())),
                ("Sound cards", Some(inv.sound.len().to_string())),
            ],
            110.,
            t,
        ))
    }

    fn software_card(&self, meta: Option<&Meta>, cx: &mut Context<Self>) -> Div {
        let card = self.card(IconName::AppWindow, "Software", Category::Software, cx);
        let t = theme(cx);
        let Some(inv) = &self.inventory else {
            return card.child(pending("Reading…", t));
        };
        let system: Option<schema::system::SystemMeta> = module(meta, "system");
        let now = hardware::now_s() as u64;
        card.child(facts(
            vec![
                ("System", system.and_then(|s| s.distro).or_else(|| inv.os.clone())),
                ("Kernel", inv.kernel.as_deref().map(hardware::kernel_summary)),
                ("Desktop", inv.desktop.clone()),
                ("Last boot", inv.boot_time.map(|b| clock_time(b, "%-d %b %Y at %H:%M"))),
                ("Uptime", inv.boot_time.map(|b| hardware::uptime(now.saturating_sub(b)))),
                ("Secure Boot", Some(hardware::secure_boot_label(inv))),
            ],
            90.,
            t,
        ))
    }

    fn sensors_card(&self, meta: Option<&Meta>, sample: Option<&Sample>, cx: &mut Context<Self>) -> Div {
        let card = self.card(IconName::Thermometer, "Sensors", Category::Sensors, cx);
        let t = theme(cx);
        let hwmon: Option<HwmonMeta> = module(meta, "hwmon");
        let sensors = hardware::sensors(sample, hwmon.as_ref());
        if sensors.is_empty() {
            return card.child(pending(
                if sample.is_some() {
                    "No readable sensors."
                } else {
                    "Waiting for the collector…"
                },
                t,
            ));
        }
        let live = self.live.read(cx);
        let now = sample.map_or_else(hardware::now_s, |s| s.t);
        let to = now as i64;
        let from = to - CHART_SPAN_S as i64;
        let width = bucket_width(from, to, CHART_BUCKETS);
        let interval = meta.and_then(|m| m.modules.get("hwmon")).map_or(2, |m| m.interval_s) as f64;
        let gap = (width as f64 * 1.5).max(interval * 2.5);
        let mut domains: Vec<(Domain, Vec<&Sensor>)> = Vec::new();
        for s in &sensors {
            match domains.last_mut() {
                Some((d, list)) if *d == s.domain => list.push(s),
                _ => domains.push((s.domain, vec![s])),
            }
        }
        let blocks: Vec<AnyElement> = domains
            .into_iter()
            .map(|(domain, list)| {
                let source = Source::new(list.iter().map(|s| s.path.clone()).collect(), Combine::Max);
                let seed = self.seed.get(&domain).map_or(&[][..], Vec::as_slice);
                let data: Rc<[Bucket]> = source.top_up(seed, live, from, width).into();
                let peak = data.iter().map(|b| b.max).fold(list[0].value, f64::max);
                let state = self.charts.borrow_mut().entry(domain).or_default().clone();
                let hottest = list[0];
                let fans = domain == Domain::Fans;
                let (value, colour) = if fans {
                    let rpm = list.iter().map(|s| s.value).fold(0., f64::max);
                    if rpm == 0. {
                        ("Off".to_owned(), t.text_secondary)
                    } else {
                        (format!("{rpm:.0} rpm"), t.teal)
                    }
                } else {
                    (
                        format!("{:.0}°C", hottest.value),
                        heat_colour(hardware::heat(hottest.value, hottest.limit)),
                    )
                };
                let mut chart = TrendChart::new(state, self.version, (from as f64, now), width as f64, gap)
                    .line(data, if fans { t.teal } else { colour }, "")
                    .relative_axis()
                    .height(px(SENSOR_CHART_HEIGHT));
                chart = if fans {
                    chart
                        .unit(Unit::Count)
                        .y(YScale::Fixed(0., hardware::fan_axis_top(peak)))
                        .compact_axis(px(34.), |v| format!("{v:.0}"))
                } else {
                    chart
                        .unit(Unit::Celsius)
                        .y(YScale::Fixed(0., hardware::temp_axis_top(peak)))
                        .compact_axis(px(30.), |v| format!("{v:.0}°"))
                };
                div()
                    .id(SharedString::from(format!("sensor-{}", domain.title())))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .min_w_0()
                    .cursor_pointer()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.))
                            .text_size(t.small_caption)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(Icon::new(domain_icon(domain)).xsmall().text_color(t.text_secondary))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(domain.title()),
                            )
                            .child(div().font_features(t.numbers.clone()).text_color(colour).child(value)),
                    )
                    .child(chart)
                    .on_click(cx.listener(move |this, _, window, cx| this.open_sensors(domain, window, cx)))
                    .into_any_element()
            })
            .collect();
        let mut grid = div().flex().flex_col().gap(px(14.));
        let mut blocks = blocks.into_iter().peekable();
        while blocks.peek().is_some() {
            let mut row = div().flex().flex_row().gap(px(14.));
            for _ in 0..SENSOR_COLUMNS {
                row = row.child(div().flex_1().min_w_0().children(blocks.next()));
            }
            grid = grid.child(row);
        }
        card.child(grid).child(
            div()
                .text_size(t.small_caption)
                .text_color(t.text_secondary)
                .child(SENSOR_NOTE),
        )
    }
}

fn caption_upper(text: &str, t: &Theme) -> impl IntoElement + use<> {
    div()
        .text_size(t.card_label)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.text_secondary)
        .child(text.to_uppercase())
}

/// A health ring: a faint full circle with the share drawn over it from twelve o'clock.
fn ring(fraction: Option<f64>, colour: Hsla, t: &Theme) -> impl IntoElement + use<> {
    let track = t.card_hover;
    let label = fraction.map_or("–".to_owned(), |f| format!("{:.0}%", f * 100.));
    div()
        .size(px(72.))
        .flex_shrink_0()
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .font_features(t.numbers.clone())
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let c = bounds.center();
                    let r = bounds.size.width.min(bounds.size.height) / 2. - px(5.);
                    let arc = |to: f64| {
                        let mut path = PathBuilder::stroke(px(9.));
                        let steps = 96;
                        for i in 0..=steps {
                            let a = (i as f64 / steps as f64 * to - 0.25) * std::f64::consts::TAU;
                            let p = point(c.x + r * a.cos() as f32, c.y + r * a.sin() as f32);
                            if i == 0 {
                                path.move_to(p);
                            } else {
                                path.line_to(p);
                            }
                        }
                        path.build().ok()
                    };
                    if let Some(p) = arc(1.) {
                        window.paint_path(p, track);
                    }
                    if let Some(p) = fraction.filter(|f| *f > 0.).and_then(|f| arc(f.min(1.))) {
                        window.paint_path(p, colour);
                    }
                },
            )
            .absolute()
            .size_full(),
        )
        .child(label)
}

impl Render for Hardware {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_search(window, cx);
        #[cfg(debug_assertions)]
        if self.built_with_data
            && self.version > 3
            && let Some(y) = self.dev_scroll.take()
        {
            self.scroll.set_offset(point(px(0.), px(-y)));
        }
        #[cfg(debug_assertions)]
        if self.built_with_data
            && self.version > 3
            && !self.dev_sensor_opened
            && let Ok(name) = std::env::var("SYSMAN_DEV_HW_SENSOR")
        {
            self.dev_sensor_opened = true;
            let sensors = hardware::sensors(self.live.read(cx).latest.as_deref(), None);
            if let Some(d) = sensors
                .iter()
                .map(|s| s.domain)
                .find(|d| d.title().eq_ignore_ascii_case(&name))
            {
                self.open_sensors(d, window, cx);
            }
        }
        let header = self.header(cx).into_any_element();
        let sidebar = sidebar::showing(&self.outline, cx).then(|| self.sidebar(cx).into_any_element());
        let detail = match self.selected {
            Selection::Overview => self.overview(window, cx),
            Selection::Category(c) => self.detail(c, None, cx),
            Selection::Node(c, i) => self.detail(c, Some(i), cx),
        };
        let t = theme(cx);
        div().size_full().flex().flex_col().child(header).child(
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_row()
                .child(sidebar::sidebar(
                    &self.outline,
                    sidebar.map(|sidebar| {
                        div()
                            .id("hw-sidebar")
                            .size_full()
                            .overflow_y_scroll()
                            .bg(t.card.opacity(0.6))
                            .border_r_1()
                            .border_color(t.hairline)
                            .p(px(10.))
                            .child(sidebar)
                            .into_any_element()
                    }),
                    window,
                    cx,
                ))
                .child(
                    div()
                        .id("hw-detail")
                        .track_scroll(&self.scroll)
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .overflow_y_scroll()
                        .child(div().p(t.page_padding).child(detail)),
                ),
        )
    }
}

/// The live list behind one sensor chart, hottest first.
struct SensorSheet {
    live: Entity<Live>,
    domain: Domain,
}

impl SensorSheet {
    fn new(live: Entity<Live>, domain: Domain, cx: &mut Context<Self>) -> Self {
        cx.observe(&live, |_, _, cx| cx.notify()).detach();
        Self { live, domain }
    }
}

impl Render for SensorSheet {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let live = self.live.read(cx);
        let hwmon: Option<HwmonMeta> = module(live.meta.as_ref(), "hwmon");
        let list: Vec<Sensor> = hardware::sensors(live.latest.as_deref(), hwmon.as_ref())
            .into_iter()
            .filter(|s| s.domain == self.domain)
            .collect();
        let fans = self.domain == Domain::Fans;
        let noun = if fans { "fan" } else { "sensor" };
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(div().text_size(t.caption).text_color(t.text_secondary).child(format!(
                "{} {noun}{}, live",
                list.len(),
                if list.len() == 1 { "" } else { "s" }
            )))
            .child(
                div()
                    .id("sensor-sheet")
                    .max_h(px(420.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(7.))
                    .children(list.into_iter().map(|s| {
                        let heat = hardware::heat(s.value, s.limit);
                        let colour = if fans { t.teal } else { heat_colour(heat) };
                        let (value, fraction) = if fans {
                            (
                                if s.value == 0. {
                                    "Off".to_owned()
                                } else {
                                    format!("{:.0} rpm", s.value)
                                },
                                (s.value / 3000.).clamp(0., 1.),
                            )
                        } else {
                            (format!("{:.1} °C", s.value), heat.max(0.02))
                        };
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(12.))
                            .text_size(px(12.))
                            .child(
                                div()
                                    .w(px(200.))
                                    .flex_shrink_0()
                                    .flex()
                                    .flex_col()
                                    .child(div().font_family(t.mono.clone()).child(s.label.clone()))
                                    .child(
                                        div()
                                            .text_size(t.small_caption)
                                            .text_color(t.text_tertiary)
                                            .child(s.chip.clone()),
                                    ),
                            )
                            .child(div().flex_1().child(capacity_bar(fraction, colour, 7., t)))
                            .child(
                                div()
                                    .w(px(70.))
                                    .flex()
                                    .justify_end()
                                    .font_features(t.numbers.clone())
                                    .text_color(colour)
                                    .child(value),
                            )
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(category: Category, titles: &[&str]) -> Section {
        Section {
            category,
            note: None,
            nodes: titles
                .iter()
                .map(|t| Node {
                    title: (*t).to_owned(),
                    subtitle: None,
                    props: vec![("Driver".into(), format!("{}-drv", t.to_lowercase()))],
                })
                .collect(),
        }
    }

    #[test]
    fn search_keeps_only_matching_nodes_but_a_matching_category_keeps_all() {
        let sections = vec![
            section(Category::Usb, &["Arctis Nova 7", "USB Receiver"]),
            section(Category::Audio, &["Arctis Nova 7", "HDA NVidia"]),
            section(Category::Pci, &["Root Complex"]),
        ];
        assert_eq!(
            filter(&sections, ""),
            vec![(0, vec![0, 1]), (1, vec![0, 1]), (2, vec![0])]
        );
        assert_eq!(filter(&sections, "arctis"), vec![(0, vec![0]), (1, vec![0])]);
        assert_eq!(
            filter(&sections, "audio"),
            vec![(1, vec![0, 1])],
            "the category title matched"
        );
        assert_eq!(
            filter(&sections, "hda nvidia-drv"),
            vec![(1, vec![1])],
            "property values match too"
        );
        assert!(filter(&sections, "nothing like this").is_empty());
    }

    #[test]
    fn heat_colour_runs_green_to_red() {
        assert!((heat_colour(0.).h * 360. - 135.).abs() < 0.5);
        assert!(heat_colour(1.).h.abs() < 0.001);
    }
}
