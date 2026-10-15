use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use gpui::{
    AnyView, App, AppContext, Context, Entity, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement,
    Pixels, Render, SharedString, StatefulInteractiveElement, Styled, Window, WindowControlArea, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::tooltip::Tooltip;
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;

use crate::data::helper;
use crate::data::live::{self, Conn, Event, Live};
use crate::data::process::{self, Interval, ProcessFeed};
use crate::data::systemd;
use crate::screens::dashboard::Dashboard;
use crate::screens::disk::Disk;
use crate::screens::gpu::Gpu;
use crate::screens::groups::Groups;
use crate::screens::network::Network;
use crate::screens::processes::Processes;
use crate::ui::settings;
use crate::ui::theme::{Theme, theme};
use crate::ui::widgets::hairline;

const TITLE_BAR_HEIGHT: Pixels = px(52.);
const STRIP_HEIGHT: Pixels = px(36.);
const STRIP_INSET: Pixels = px(3.);
const TAB_PADDING_X: Pixels = px(20.);
const TAB_MIN_WIDTH: Pixels = px(64.);
const CONTROL_HEIGHT: Pixels = px(32.);
const TAB_FONT: Pixels = px(13.);
/// Room each side of the centred strip: the refresh and settings controls and their margin.
const SIDE_WIDTH: Pixels = px(150.);
const DRAIN_EVERY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tab {
    Dashboard,
    Processes,
    Gpu,
    Energy,
    Network,
    Disk,
    Hardware,
    Explorer,
    Insights,
    Groups,
}

impl Tab {
    pub const ALL: [Tab; 10] = [
        Self::Dashboard,
        Self::Processes,
        Self::Gpu,
        Self::Energy,
        Self::Network,
        Self::Disk,
        Self::Hardware,
        Self::Explorer,
        Self::Insights,
        Self::Groups,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Dashboard => "Dashboard",
            Self::Processes => "Processes",
            Self::Gpu => "GPU",
            Self::Energy => "Energy",
            Self::Network => "Network",
            Self::Disk => "Disk",
            Self::Hardware => "Hardware",
            Self::Explorer => "Explorer",
            Self::Insights => "Insights",
            Self::Groups => "Groups",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::Dashboard => IconName::Gauge,
            Self::Processes => IconName::List,
            Self::Gpu => IconName::Monitor,
            Self::Energy => IconName::Zap,
            Self::Network => IconName::Network,
            Self::Disk => IconName::HardDrive,
            Self::Hardware => IconName::Cpu,
            Self::Explorer => IconName::Activity,
            Self::Insights => IconName::Lightbulb,
            Self::Groups => IconName::Layers,
        }
    }

    /// The live number each placeholder shows: label, series path, unit.
    fn probe(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Dashboard => ("CPU", "/cpu/total_pct", "%"),
            Self::Processes => ("Processes", "/processes/count", ""),
            Self::Hardware => ("CPU temperature", "/cpu/temp_c", " °C"),
            Self::Explorer => ("Load (1 min)", "/system/load/0", ""),
            Self::Insights => ("OOM kills", "/memory/oom_kills_total", ""),
            Self::Groups => ("Tasks", "/system/tasks_total", ""),
            Self::Gpu | Self::Energy | Self::Network | Self::Disk => ("Memory used", "/memory/used_pct", "%"),
        }
    }
}

/// Redraw throttle; collection always runs at the daemon's own interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refresh(Duration);

impl Refresh {
    pub const CHOICES: [Refresh; 5] = [
        Refresh(Duration::from_secs(2)),
        Refresh(Duration::from_secs(5)),
        Refresh(Duration::from_secs(10)),
        Refresh(Duration::from_secs(30)),
        Refresh(Duration::from_secs(60)),
    ];

    pub fn label(self) -> SharedString {
        match self.0.as_secs() {
            60 => "1 min".into(),
            s => format!("{s} s").into(),
        }
    }
}

pub struct Shell {
    tab: Tab,
    active: AnyView,
    live: Entity<Live>,
    events: Option<Receiver<Event>>,
    feed: Entity<ProcessFeed>,
    feed_events: Option<Receiver<process::Event>>,
    feed_interval: Interval,
    refresh: Refresh,
    drawn: Instant,
    pending: bool,
    helper_running: bool,
}

impl Shell {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let live = cx.new(|_| Live::default());
        settings::install(live.clone(), cx.entity().downgrade(), Arc::new(systemd::System), cx);
        cx.observe(&live, |_, _, cx| cx.notify()).detach();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(DRAIN_EVERY).await;
                if this.update(cx, |shell, cx| shell.drain(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        let feed = cx.new(|_| ProcessFeed::default());
        let feed_interval = Interval::default();
        #[cfg(debug_assertions)]
        let tab = std::env::var("SYSMAN_DEV_TAB")
            .ok()
            .and_then(|name| Tab::ALL.into_iter().find(|t| t.label().eq_ignore_ascii_case(&name)))
            .unwrap_or(Tab::Dashboard);
        #[cfg(not(debug_assertions))]
        let tab = Tab::Dashboard;
        let shell = Self {
            tab,
            active: mount(tab, &live, &feed, cx),
            events: live::socket_path().map(live::spawn),
            feed_events: live::socket_path().map(|path| process::spawn(path, feed_interval.clone())),
            feed_interval,
            feed,
            live,
            refresh: Refresh::CHOICES[0],
            drawn: Instant::now(),
            pending: false,
            helper_running: false,
        };
        shell.sync_feed();
        shell
    }

    /// Only the tabs that show the process table pay for the daemon's scan; Network shows it only via the helper.
    fn sync_feed(&self) {
        let ms = match self.tab {
            Tab::Dashboard | Tab::Processes | Tab::Disk | Tab::Groups => self.refresh.0.as_millis() as u64,
            Tab::Network if self.helper_running => self.refresh.0.as_millis() as u64,
            _ => 0,
        };
        self.feed_interval.store(ms, Ordering::Relaxed);
    }

    /// For the first-frame aid: the mounted Dashboard has a sample and its history.
    #[cfg(debug_assertions)]
    pub fn ready(&self, cx: &App) -> bool {
        let active = self.active.clone();
        if let Ok(v) = active.clone().downcast::<Dashboard>() {
            return v.read(cx).ready(cx);
        }
        if let Ok(v) = active.clone().downcast::<Processes>() {
            return v.read(cx).ready(cx);
        }
        if let Ok(v) = active.clone().downcast::<Gpu>() {
            return v.read(cx).ready(cx);
        }
        if let Ok(v) = active.clone().downcast::<Network>() {
            return v.read(cx).ready(cx);
        }
        active.downcast::<Disk>().is_ok_and(|v| v.read(cx).ready(cx))
    }

    fn drain(&mut self, cx: &mut Context<Self>) {
        if let Some(events) = &self.feed_events {
            self.feed.update(cx, |feed, cx| {
                let mut changed = false;
                for event in events.try_iter() {
                    changed = true;
                    feed.apply(event);
                }
                if changed {
                    cx.notify();
                }
            });
        }
        let Some(events) = &self.events else {
            return;
        };
        let mut reconnected = false;
        let mut data = false;
        self.live.update(cx, |live, _| {
            for event in events.try_iter() {
                match &event {
                    Event::Conn(conn) => reconnected |= live.conn != *conn,
                    Event::Record { .. } | Event::Skipped => data = true,
                }
                live.apply(event);
            }
        });
        self.pending |= data;
        let running = helper::running(self.live.read(cx).latest.as_ref());
        if running != self.helper_running {
            self.helper_running = running;
            self.sync_feed();
        }
        if reconnected || (self.pending && self.drawn.elapsed() >= self.refresh.0) {
            self.pending = false;
            self.drawn = Instant::now();
            self.live.update(cx, |_, cx| cx.notify());
        }
    }

    pub fn refresh(&self) -> Refresh {
        self.refresh
    }

    pub fn set_refresh(&mut self, refresh: Refresh, cx: &mut Context<Self>) {
        self.refresh = refresh;
        self.sync_feed();
        cx.notify();
    }

    fn select(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if tab != self.tab {
            self.tab = tab;
            self.sync_feed();
            self.active = mount(tab, &self.live, &self.feed, cx);
            cx.notify();
        }
    }

    fn title_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let compact = strip_width(window, t) > window.viewport_size().width - SIDE_WIDTH * 2.;
        let gear = pill(Button::new("settings").ghost().icon(Icon::new(IconName::Settings)), t)
            .w(CONTROL_HEIGHT)
            .tooltip("Settings")
            .on_click(|_, _, cx| settings::open(cx));
        let drag = |id: &'static str| {
            div()
                .id(id)
                .flex_1()
                .h_full()
                .window_control_area(WindowControlArea::Drag)
                .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .h(TITLE_BAR_HEIGHT)
            .child(drag("drag-left"))
            .child(self.tab_strip(compact, cx))
            .child(
                drag("drag-right")
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .pr(px(14.))
                    .gap(px(6.))
                    .child(self.refresh_control(cx))
                    .child(gear),
            )
    }

    fn tab_strip(&self, compact: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let active = Tab::ALL.iter().position(|&tab| tab == self.tab).unwrap_or(0);
        hairline(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .h(STRIP_HEIGHT)
                .p(STRIP_INSET)
                .rounded_full()
                .bg(t.strip)
                .border_color(t.strip_border),
            t.hairline_width,
        )
        .children(Tab::ALL.iter().enumerate().map(|(i, &tab)| {
            let selected = i == active;
            // A divider only between two unselected segments, as in a native segmented control.
            let divider = i > 0 && !selected && i != active + 1;
            div()
                .id(tab.label())
                .flex()
                .flex_row()
                .items_center()
                .child(div().w(px(1.)).h(px(14.)).when(divider, |d| d.bg(t.hairline)))
                .child(
                    div()
                        .id(("tab", i))
                        .h(STRIP_HEIGHT - STRIP_INSET * 2.)
                        .min_w(if compact { px(38.) } else { TAB_MIN_WIDTH })
                        .px(if compact { px(11.) } else { TAB_PADDING_X })
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .text_size(TAB_FONT)
                        .font_weight(FontWeight::MEDIUM)
                        .when(selected, |d| d.bg(t.strip_active).text_color(t.text))
                        .when(!selected, |d| {
                            d.text_color(t.text_secondary).hover(|d| d.text_color(t.text))
                        })
                        .map(|d| {
                            if compact {
                                d.child(Icon::new(tab.icon()).size_4())
                                    .tooltip(move |window, cx| Tooltip::new(tab.label()).build(window, cx))
                            } else {
                                d.child(tab.label())
                            }
                        })
                        .on_click(cx.listener(move |shell, _, _, cx| shell.select(tab, cx))),
                )
        }))
    }

    fn refresh_control(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let shell = cx.entity().downgrade();
        let current = self.refresh;
        let t = theme(cx);
        pill(
            Button::new("refresh")
                .ghost()
                .small()
                .icon(Icon::new(IconName::Timer))
                .label(current.label())
                .child(Icon::new(IconName::ChevronDown).xsmall().text_color(t.text_secondary)),
            t,
        )
        .px(px(12.))
        .tooltip("Redraw interval")
        .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |mut menu, _, _| {
            for choice in Refresh::CHOICES {
                let shell = shell.clone();
                menu = menu.item(PopupMenuItem::new(choice.label()).checked(choice == current).on_click(
                    move |_, _, cx| {
                        shell.update(cx, |shell, cx| shell.set_refresh(choice, cx)).ok();
                    },
                ));
            }
            menu
        })
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let waiting = self.live.read(cx).conn == Conn::Waiting;
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.window)
            .text_color(t.text)
            .font_family(t.font.clone())
            .child(self.title_bar(window, cx))
            .when(waiting, |d| d.child(banner(cx)))
            .child(div().flex_1().min_h_0().child(self.active.clone()))
    }
}

/// The bordered round control of the title bar's right side.
fn pill(button: Button, t: &Theme) -> Button {
    hairline(
        button
            .h(CONTROL_HEIGHT)
            .rounded_full()
            .bg(t.control)
            .border_color(t.control_border),
        t.hairline_width,
    )
}

fn mount(tab: Tab, live: &Entity<Live>, feed: &Entity<ProcessFeed>, cx: &mut App) -> AnyView {
    let (live, feed) = (live.clone(), feed.clone());
    match tab {
        Tab::Dashboard => cx.new(|cx| Dashboard::new(live, feed, cx)).into(),
        Tab::Processes => cx.new(|cx| Processes::new(live, feed, cx)).into(),
        Tab::Gpu => cx.new(|cx| Gpu::new(live, cx)).into(),
        Tab::Network => cx.new(|cx| Network::new(live, feed, cx)).into(),
        Tab::Disk => cx.new(|cx| Disk::new(live, feed, cx)).into(),
        Tab::Groups => cx.new(|cx| Groups::new(live, feed, cx)).into(),
        _ => cx.new(|_| Placeholder { tab, live }).into(),
    }
}

/// Width the strip needs with labels, so it can fall back to icons before it would overflow.
fn strip_width(window: &mut Window, t: &Theme) -> Pixels {
    let mut style = window.text_style();
    style.font_family = t.font.clone();
    Tab::ALL.iter().fold(STRIP_INSET * 2. + px(1.), |total, tab| {
        let run = style.to_run(tab.label().len());
        let text = window
            .text_system()
            .shape_line(tab.label().into(), TAB_FONT, &[run], None)
            .width;
        total + (text + TAB_PADDING_X * 2.).max(TAB_MIN_WIDTH) + px(1.)
    })
}

fn banner(cx: &App) -> impl IntoElement {
    let t = theme(cx);
    let path = live::socket_path().map_or("XDG_RUNTIME_DIR is not set".into(), |p| p.display().to_string());
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px(t.page_padding)
        .py(px(7.))
        .bg(t.orange.opacity(0.14))
        .border_b_1()
        .border_color(t.orange.opacity(0.3))
        .text_size(t.caption)
        .child(div().size(px(6.)).rounded_full().bg(t.orange))
        .child(div().font_weight(FontWeight::SEMIBOLD).child("Collector not running"))
        .child(
            div()
                .text_color(t.text_secondary)
                .child(format!("waiting for sysmand at {path}")),
        )
}

struct Placeholder {
    tab: Tab,
    live: Entity<Live>,
}

impl Render for Placeholder {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let (label, path, unit) = self.tab.probe();
        let value = self.live.read(cx).value(path);
        let value = value.map_or("–".to_string(), |v| {
            if v.fract() == 0.0 {
                format!("{v:.0}{unit}")
            } else {
                format!("{v:.1}{unit}")
            }
        });
        div().size_full().p(t.page_padding).child(
            div()
                .flex()
                .flex_col()
                .gap(px(14.))
                .p(t.panel_padding)
                .rounded(t.panel_radius)
                .bg(t.panel)
                .border_color(t.hairline)
                .map(|mut d| {
                    let w = Some(t.hairline_width.into());
                    let edges = &mut d.style().border_widths;
                    (edges.top, edges.right, edges.bottom, edges.left) = (w, w, w, w);
                    d
                })
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(7.))
                        .text_size(t.panel_title)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(Icon::new(self.tab.icon()).size_4().text_color(t.text_secondary))
                        .child(self.tab.label()),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(t.card_label)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(t.text_secondary)
                                .child(label.to_uppercase()),
                        )
                        .child(
                            div()
                                .text_size(t.card_value)
                                .font_weight(FontWeight::SEMIBOLD)
                                .font_features(t.numbers.clone())
                                .child(value),
                        ),
                ),
        )
    }
}
