use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use gpui::{
    AnyView, App, AppContext, Context, Decorations, Entity, Global, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, ParentElement, Pixels, Rems, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, WeakEntity, Window, WindowControlArea, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::tooltip::Tooltip;
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;
use serde::{Deserialize, Serialize};

use crate::charts::format::TempUnit;
use crate::data::helper;
use crate::data::live::{self, Conn, Event, Live};
use crate::data::prefs::prefs;
use crate::data::process::{self, Interval, ProcessFeed};
use crate::data::systemd;
use crate::screens::dashboard::Dashboard;
use crate::screens::disk::Disk;
use crate::screens::energy::Energy;
use crate::screens::explorer::Explorer;
use crate::screens::gpu::Gpu;
use crate::screens::groups::Groups;
use crate::screens::hardware::Hardware;
use crate::screens::insights::Insights;
use crate::screens::network::Network;
use crate::screens::processes::Processes;
use crate::ui::settings;
use crate::ui::theme::{self, TextSize, Theme, rem_px, theme};
use crate::ui::widgets::hairline;

const TITLE_BAR_HEIGHT: Rems = rem_px(52.);
const STRIP_HEIGHT: Rems = rem_px(36.);
const STRIP_INSET: Rems = rem_px(3.);
const TAB_PADDING_X: Rems = rem_px(20.);
const TAB_MIN_WIDTH: Rems = rem_px(64.);
pub(crate) const CONTROL_HEIGHT: Rems = rem_px(32.);
const TAB_FONT: Rems = rem_px(13.);
/// Room each side of the centred strip: the refresh and settings controls and their margin.
const SIDE_WIDTH: Rems = rem_px(150.);
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
}

/// What the screen opened by [`navigate`] should select once its data is in.
#[derive(Debug, Clone, PartialEq)]
pub enum Focus {
    /// This instance if it still runs, else the newest process with this command.
    Process { comm: String, instance: Option<(u32, u64)> },
    /// The disk that holds this mount point.
    Mount(String),
}

struct Navigator(WeakEntity<Shell>);

impl Global for Navigator {}

struct PendingFocus(Option<Focus>);

impl Global for PendingFocus {}

/// Switches tabs from inside a screen; the screen mounted there takes `focus` with [`take_focus`].
pub fn navigate(tab: Tab, focus: Option<Focus>, cx: &mut App) {
    cx.set_global(PendingFocus(focus));
    if let Some(shell) = cx.try_global::<Navigator>().map(|n| n.0.clone()) {
        shell.update(cx, |shell, cx| shell.select(tab, cx)).ok();
    }
}

pub fn take_focus(cx: &mut App) -> Option<Focus> {
    cx.has_global::<PendingFocus>()
        .then(|| cx.global_mut::<PendingFocus>().0.take())
        .flatten()
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

    const KEY: &str = "app.refresh";

    pub fn label(self) -> SharedString {
        match self.0.as_secs() {
            60 => "1 min".into(),
            s => format!("{s} s").into(),
        }
    }

    /// Stored as seconds; anything else is the first choice.
    fn load() -> Self {
        Self::stored(prefs().lock().ok().and_then(|p| p.get::<u64>(Self::KEY)))
    }

    fn stored(secs: Option<u64>) -> Self {
        Self::CHOICES
            .into_iter()
            .find(|c| Some(c.0.as_secs()) == secs)
            .unwrap_or(Self::CHOICES[0])
    }

    fn save(self) {
        if let Ok(mut p) = prefs().lock() {
            p.set(Self::KEY, &self.0.as_secs());
        }
    }
}

/// Whether empty title-bar space moves the window; `Never` is for compositors that do not report tiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TitlebarDrag {
    #[default]
    Auto,
    Never,
}

impl TitlebarDrag {
    pub const ALL: [TitlebarDrag; 2] = [Self::Auto, Self::Never];
    const KEY: &str = "display.titlebar_drag";

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Never => "Never",
        }
    }

    fn load() -> Self {
        prefs().lock().ok().and_then(|p| p.get(Self::KEY)).unwrap_or_default()
    }

    fn save(self) {
        if let Ok(mut p) = prefs().lock() {
            p.set(Self::KEY, &self);
        }
    }
}

/// What a press on empty title-bar space does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Press {
    Move,
    Maximize,
    Nothing,
}

/// While the compositor lays the window out (any tiled edge, maximised, fullscreen), a move request makes a tiled
/// window jump or float, so the title bar leaves it alone.
fn title_bar_press(
    setting: TitlebarDrag,
    decorations: Decorations,
    maximized: bool,
    fullscreen: bool,
    clicks: usize,
) -> Press {
    let tiled = matches!(decorations, Decorations::Client { tiling } if tiling.is_tiled());
    if setting == TitlebarDrag::Never || tiled || maximized || fullscreen {
        Press::Nothing
    } else if clicks >= 2 {
        Press::Maximize
    } else {
        Press::Move
    }
}

/// How long the window may stay unfocused before live work pauses; `None` never pauses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PauseAfter(Option<Duration>);

impl PauseAfter {
    pub const CHOICES: [PauseAfter; 5] = [
        PauseAfter(Some(Duration::from_secs(60))),
        PauseAfter(Some(Duration::from_secs(180))),
        PauseAfter(Some(Duration::from_secs(300))),
        PauseAfter(Some(Duration::from_secs(600))),
        PauseAfter(None),
    ];
    const KEY: &str = "app.pause_after";

    pub fn label(self) -> SharedString {
        match self.0 {
            Some(d) => format!("After {} min", d.as_secs() / 60).into(),
            None => "Never".into(),
        }
    }

    /// Stored as seconds, 0 for never.
    fn load() -> Self {
        let secs = prefs().lock().ok().and_then(|p| p.get::<u64>(Self::KEY));
        Self::CHOICES
            .into_iter()
            .find(|c| Some(c.0.map_or(0, |d| d.as_secs())) == secs)
            .unwrap_or(Self::CHOICES[0])
    }

    fn save(self) {
        if let Ok(mut p) = prefs().lock() {
            p.set(Self::KEY, &self.0.map_or(0, |d| d.as_secs()));
        }
    }
}

/// Whether live work should stop: at once while the window is hidden, or once it has been unfocused for `after`.
#[derive(Debug, Clone, Copy)]
struct Idle {
    after: Option<Duration>,
    visible: bool,
    unfocused_since: Option<Instant>,
}

impl Idle {
    fn new(after: Option<Duration>) -> Self {
        Self {
            after,
            visible: true,
            unfocused_since: None,
        }
    }

    fn focus(&mut self, focused: bool, now: Instant) {
        if focused {
            self.unfocused_since = None;
        } else if self.unfocused_since.is_none() {
            self.unfocused_since = Some(now);
        }
    }

    fn paused(&self, now: Instant) -> bool {
        !self.visible
            || self
                .after
                .zip(self.unfocused_since)
                .is_some_and(|(after, since)| now.duration_since(since) >= after)
    }
}

/// Set while live work is paused, for the animations that would otherwise keep asking for frames.
struct Paused(bool);

impl Global for Paused {}

pub fn paused(cx: &App) -> bool {
    cx.try_global::<Paused>().is_some_and(|p| p.0)
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
    /// A process table arrived since the last redraw; it is shown with the next one, so both cost one frame.
    feed_pending: bool,
    /// The next process table is shown on arrival: a newly opened tab should not wait a whole interval for it.
    feed_eager: bool,
    helper_running: bool,
    pause_after: PauseAfter,
    drag: TitlebarDrag,
    idle: Idle,
    paused: bool,
    /// Window activation and visibility observers, made on the first render, which is the first to see the window.
    watching: Vec<Subscription>,
    #[cfg(debug_assertions)]
    frames: Option<(Instant, u32)>,
}

impl Shell {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let navigator = Navigator(cx.weak_entity());
        cx.set_global(navigator);
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
        let pause_after = PauseAfter::load();
        #[cfg(debug_assertions)]
        let idle_after = std::env::var("SYSMAN_DEV_PAUSE_AFTER_S")
            .ok()
            .and_then(|s| s.parse().ok())
            .map_or(pause_after.0, |s| Some(Duration::from_secs(s)));
        #[cfg(not(debug_assertions))]
        let idle_after = pause_after.0;
        let shell = Self {
            tab,
            active: mount(tab, &live, &feed, cx),
            events: live::socket_path().map(live::spawn),
            feed_events: live::socket_path().map(|path| process::spawn(path, feed_interval.clone())),
            feed_interval,
            feed,
            live,
            refresh: Refresh::load(),
            drawn: Instant::now(),
            pending: false,
            feed_pending: false,
            feed_eager: true,
            helper_running: false,
            pause_after,
            drag: TitlebarDrag::load(),
            idle: Idle::new(idle_after),
            paused: false,
            watching: Vec::new(),
            #[cfg(debug_assertions)]
            frames: std::env::var_os("SYSMAN_DEV_FRAMES").map(|_| (Instant::now(), 0)),
        };
        shell.sync_feed();
        shell
    }

    /// Only the tabs that show the process table pay for the daemon's scan; Network shows it only via the helper.
    fn sync_feed(&self) {
        let ms = match self.tab {
            _ if self.paused => 0,
            Tab::Dashboard | Tab::Processes | Tab::Disk | Tab::Groups | Tab::Insights => {
                self.refresh.0.as_millis() as u64
            }
            Tab::Network if self.helper_running => self.refresh.0.as_millis() as u64,
            _ => 0,
        };
        self.feed_interval.store(ms, Ordering::Relaxed);
    }

    pub fn pause_after(&self) -> PauseAfter {
        self.pause_after
    }

    pub fn set_pause_after(&mut self, choice: PauseAfter, cx: &mut Context<Self>) {
        self.pause_after = choice;
        self.idle.after = choice.0;
        choice.save();
        self.update_pause(cx);
        cx.notify();
    }

    pub fn titlebar_drag(&self) -> TitlebarDrag {
        self.drag
    }

    pub fn set_titlebar_drag(&mut self, choice: TitlebarDrag, cx: &mut Context<Self>) {
        self.drag = choice;
        choice.save();
        cx.notify();
    }

    /// Forgets sidebar widths, column choices and the like; the open tab is rebuilt to show the defaults.
    pub fn reset_layout(&mut self, cx: &mut Context<Self>) {
        if let Ok(mut p) = prefs().lock() {
            p.retain(|key| key.starts_with("app.") || key.starts_with("display."));
        }
        self.active = mount(self.tab, &self.live, &self.feed, cx);
        cx.notify();
    }

    /// Takes up preferences that were edited by hand in `ui.toml`, already reloaded.
    pub fn prefs_changed(&mut self, cx: &mut Context<Self>) {
        self.refresh = Refresh::load();
        self.sync_feed();
        self.pause_after = PauseAfter::load();
        self.idle.after = self.pause_after.0;
        self.update_pause(cx);
        self.drag = TitlebarDrag::load();
        TempUnit::reload();
        let size = TextSize::load();
        if size != theme(cx).text_size {
            theme::install(size, cx);
        }
        cx.refresh_windows();
    }

    fn watch_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        self.idle.focus(window.is_window_active(), now);
        self.idle.visible = window.is_visible();
        self.watching = vec![
            cx.observe_window_activation(window, |shell, window, cx| {
                shell.idle.focus(window.is_window_active(), Instant::now());
                shell.update_pause(cx);
            }),
            cx.observe_window_visibility(window, |shell, visibility, _, cx| {
                shell.idle.visible = visibility.is_visible();
                shell.update_pause(cx);
            }),
        ];
        #[cfg(debug_assertions)]
        if let Ok(spec) = std::env::var("SYSMAN_DEV_DRIVE") {
            drive(&spec, window, cx);
        }
        self.update_pause(cx);
    }

    fn update_pause(&mut self, cx: &mut Context<Self>) {
        let paused = self.idle.paused(Instant::now());
        if paused == self.paused {
            return;
        }
        self.paused = paused;
        #[cfg(debug_assertions)]
        if self.frames.is_some() {
            eprintln!("SYSMAN_DEV_FRAMES: {}", if paused { "paused" } else { "resumed" });
        }
        cx.set_global(Paused(paused));
        self.sync_feed();
        if !paused {
            self.live.update(cx, |live, _| live.resumed());
            // One catch-up render of the open tab; the rings kept ingesting, so it draws from memory.
            self.redraw(true, cx);
            self.feed_eager = true;
        }
        cx.notify();
    }

    /// Hands what arrived since the last redraw to the screens, then marks the time.
    fn redraw(&mut self, live: bool, cx: &mut Context<Self>) {
        self.drawn = Instant::now();
        if std::mem::take(&mut self.feed_pending) {
            self.feed_eager = false;
            self.feed.update(cx, |_, cx| cx.notify());
        }
        if std::mem::take(&mut self.pending) || live {
            self.live.update(cx, |_, cx| cx.notify());
        }
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
        if let Ok(v) = active.clone().downcast::<Explorer>() {
            return v.read(cx).ready(cx);
        }
        if let Ok(v) = active.clone().downcast::<Energy>() {
            return v.read(cx).ready(cx);
        }
        if let Ok(v) = active.clone().downcast::<Network>() {
            return v.read(cx).ready(cx);
        }
        if let Ok(v) = active.clone().downcast::<Insights>() {
            return v.read(cx).ready(cx);
        }
        active.downcast::<Disk>().is_ok_and(|v| v.read(cx).ready(cx))
    }

    /// The rings ingest every sample even while paused; only the redraws they would trigger wait.
    fn drain(&mut self, cx: &mut Context<Self>) {
        if let Some(events) = &self.feed_events {
            let mut changed = false;
            self.feed.update(cx, |feed, _| {
                for event in events.try_iter() {
                    changed = true;
                    feed.apply(event);
                }
            });
            self.feed_pending |= changed;
        }
        let mut reconnected = false;
        if let Some(events) = &self.events {
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
        }
        let running = helper::running(self.live.read(cx).latest.as_deref());
        if running != self.helper_running {
            self.helper_running = running;
            self.sync_feed();
        }
        self.update_pause(cx);
        if self.paused {
            return;
        }
        let due = self.drawn.elapsed() >= self.refresh.0;
        if reconnected || (due && (self.pending || self.feed_pending)) {
            self.redraw(reconnected, cx);
        } else if self.feed_pending && self.feed_eager {
            self.feed_pending = false;
            self.feed_eager = false;
            self.feed.update(cx, |_, cx| cx.notify());
        }
    }

    pub fn refresh(&self) -> Refresh {
        self.refresh
    }

    pub fn set_refresh(&mut self, refresh: Refresh, cx: &mut Context<Self>) {
        self.refresh = refresh;
        refresh.save();
        self.sync_feed();
        cx.notify();
    }

    fn select(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if tab != self.tab {
            self.tab = tab;
            self.feed_eager = true;
            self.sync_feed();
            self.active = mount(tab, &self.live, &self.feed, cx);
            cx.notify();
        }
    }

    fn title_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let rem = window.rem_size();
        let compact = strip_width(window, t) > window.viewport_size().width - SIDE_WIDTH.to_pixels(rem) * 2.;
        let gear = pill(Button::new("settings").ghost().icon(Icon::new(IconName::Settings)), t)
            .w(CONTROL_HEIGHT)
            .tooltip("Settings")
            .on_click(|_, _, cx| settings::open(cx));
        let setting = self.drag;
        let drag = move |id: &'static str| {
            div()
                .id(id)
                .flex_1()
                .h_full()
                .window_control_area(WindowControlArea::Drag)
                .on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, window, _| {
                    let press = title_bar_press(
                        setting,
                        window.window_decorations(),
                        window.is_maximized(),
                        window.is_fullscreen(),
                        e.click_count,
                    );
                    match press {
                        Press::Move => window.start_window_move(),
                        Press::Maximize => window.zoom_window(),
                        Press::Nothing => {}
                    }
                })
        };
        // The controls sit inside the right drag area; a press on them ends here instead of bubbling up to it.
        let controls = div()
            .id("title-controls")
            .flex()
            .flex_row()
            .items_center()
            .gap(rem_px(6.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(self.refresh_control(cx))
            .child(gear);
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
                    .pr(rem_px(14.))
                    .child(controls),
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
                .child(div().w(px(1.)).h(rem_px(14.)).when(divider, |d| d.bg(t.hairline)))
                .child(
                    div()
                        .id(("tab", i))
                        .h(rem_px(30.))
                        .min_w(if compact { rem_px(38.) } else { TAB_MIN_WIDTH })
                        .px(if compact { rem_px(11.) } else { TAB_PADDING_X })
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .text_size(TAB_FONT)
                        .font_weight(t.weight_medium)
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
        .px(rem_px(12.))
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
        if self.watching.is_empty() {
            self.watch_window(window, cx);
        }
        #[cfg(debug_assertions)]
        if let Some((since, n)) = &mut self.frames {
            *n += 1;
            if since.elapsed() >= Duration::from_secs(5) {
                eprintln!("SYSMAN_DEV_FRAMES: {n} in {:.1} s", since.elapsed().as_secs_f64());
                (*since, *n) = (Instant::now(), 0);
            }
        }
        let t = theme(cx);
        let waiting = self.live.read(cx).conn == Conn::Waiting;
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.window)
            .text_color(t.text)
            .font_family(t.font.clone())
            .font_weight(t.weight_body)
            .child(self.title_bar(window, cx))
            .when(waiting, |d| d.child(banner(cx)))
            .child(div().flex_1().min_h_0().child(self.active.clone()))
    }
}

/// The bordered round control of the title bar's right side.
pub(crate) fn pill(button: Button, t: &Theme) -> Button {
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
        Tab::Energy => cx.new(|cx| Energy::new(live, cx)).into(),
        Tab::Groups => cx.new(|cx| Groups::new(live, feed, cx)).into(),
        Tab::Explorer => cx.new(|cx| Explorer::new(live, cx)).into(),
        Tab::Hardware => cx.new(|cx| Hardware::new(live, cx)).into(),
        Tab::Insights => cx.new(|cx| Insights::new(live, feed, cx)).into(),
    }
}

/// Width the strip needs with labels, so it can fall back to icons before it would overflow.
fn strip_width(window: &mut Window, t: &Theme) -> Pixels {
    let mut style = window.text_style();
    style.font_family = t.font.clone();
    style.font_weight = t.weight_medium;
    let rem = window.rem_size();
    let (font, padding, min) = (
        TAB_FONT.to_pixels(rem),
        TAB_PADDING_X.to_pixels(rem),
        TAB_MIN_WIDTH.to_pixels(rem),
    );
    Tab::ALL
        .iter()
        .fold(STRIP_INSET.to_pixels(rem) * 2. + px(1.), |total, tab| {
            let run = style.to_run(tab.label().len());
            let text = window
                .text_system()
                .shape_line(tab.label().into(), font, &[run], None)
                .width;
            total + (text + padding * 2.).max(min) + px(1.)
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
        .py(rem_px(7.))
        .bg(t.orange.opacity(0.14))
        .border_b_1()
        .border_color(t.orange.opacity(0.3))
        .text_size(t.caption)
        .child(div().size(rem_px(6.)).rounded_full().bg(t.orange))
        .child(div().font_weight(t.weight_semibold).child("Collector not running"))
        .child(
            div()
                .text_color(t.text_secondary)
                .child(format!("waiting for sysmand at {path}")),
        )
}

/// Development aid for measuring frame cost without input, sixty events a second: `scroll,x,y` wheels whatever is
/// under (x, y) down and back up; `hover,x0,x1,y` sweeps the pointer between x0 and x1.
#[cfg(debug_assertions)]
fn drive(spec: &str, window: &mut Window, cx: &mut Context<Shell>) {
    use gpui::{Modifiers, MouseMoveEvent, PlatformInput, ScrollDelta, ScrollWheelEvent, TouchPhase, point};
    let mut parts = spec.split(',');
    let kind = parts.next().unwrap_or_default().trim().to_owned();
    let n: Vec<f32> = parts.filter_map(|s| s.trim().parse().ok()).collect();
    cx.spawn_in(window, async move |_, cx| {
        let mut step = 0u32;
        loop {
            cx.background_executor().timer(Duration::from_millis(16)).await;
            step += 1;
            let event = match (kind.as_str(), n.as_slice()) {
                ("scroll", &[x, y]) => PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position: point(px(x), px(y)),
                    delta: ScrollDelta::Pixels(point(
                        px(0.),
                        px(if (step / 60).is_multiple_of(2) { -40. } else { 40. }),
                    )),
                    modifiers: Modifiers::default(),
                    touch_phase: TouchPhase::Moved,
                }),
                ("hover", &[x0, x1, y]) => {
                    let f = (step % 120) as f32 / 60.;
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position: point(px(x0 + (x1 - x0) * if f < 1. { f } else { 2. - f }), px(y)),
                        pressed_button: None,
                        modifiers: Modifiers::default(),
                    })
                }
                _ => {
                    eprintln!("SYSMAN_DEV_DRIVE: expected scroll,x,y or hover,x0,x1,y");
                    return;
                }
            };
            if cx.update(|window, cx| window.dispatch_event(event, cx)).is_err() {
                return;
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: Duration = Duration::from_secs(60);

    #[test]
    fn unfocused_pauses_only_once_the_chosen_time_has_passed() {
        let t0 = Instant::now();
        let mut idle = Idle::new(Some(MINUTE));
        assert!(!idle.paused(t0));
        idle.focus(false, t0);
        assert!(
            !idle.paused(t0 + MINUTE - Duration::from_millis(1)),
            "a glance at another window is not idle"
        );
        assert!(idle.paused(t0 + MINUTE));
        idle.focus(true, t0 + 2 * MINUTE);
        assert!(!idle.paused(t0 + 2 * MINUTE), "focus resumes at once");
    }

    #[test]
    fn refocusing_before_the_timer_starts_it_again() {
        let t0 = Instant::now();
        let mut idle = Idle::new(Some(MINUTE));
        idle.focus(false, t0);
        idle.focus(true, t0 + MINUTE / 2);
        idle.focus(false, t0 + MINUTE - Duration::from_secs(1));
        assert!(
            !idle.paused(t0 + MINUTE + Duration::from_secs(10)),
            "counted from the last focus loss"
        );
        assert!(idle.paused(t0 + 2 * MINUTE));
    }

    #[test]
    fn a_repeated_unfocus_does_not_restart_the_timer() {
        // activation callbacks also fire for other changes; only the first loss of focus starts the clock
        let t0 = Instant::now();
        let mut idle = Idle::new(Some(MINUTE));
        idle.focus(false, t0);
        idle.focus(false, t0 + MINUTE / 2);
        assert!(idle.paused(t0 + MINUTE));
    }

    #[test]
    fn hidden_pauses_at_once_even_when_set_to_never() {
        // nobody can see a hidden window, so its redraws are pure cost whatever the focus setting says
        let t0 = Instant::now();
        let mut idle = Idle::new(None);
        idle.focus(false, t0);
        assert!(!idle.paused(t0 + 100 * MINUTE), "never pauses for focus alone");
        idle.visible = false;
        assert!(idle.paused(t0 + 100 * MINUTE));
        idle.visible = true;
        assert!(!idle.paused(t0 + 100 * MINUTE));
    }

    #[test]
    fn the_default_is_one_minute_and_never_is_last() {
        assert_eq!(PauseAfter::CHOICES[0], PauseAfter(Some(MINUTE)));
        assert_eq!(PauseAfter::CHOICES[0].label(), "After 1 min");
        assert_eq!(PauseAfter::CHOICES[4].label(), "Never");
    }

    const FLOATING: Decorations = Decorations::Client {
        tiling: gpui::Tiling {
            top: false,
            left: false,
            right: false,
            bottom: false,
        },
    };

    #[test]
    fn a_floating_window_moves_and_maximizes_from_the_title_bar() {
        assert_eq!(
            title_bar_press(TitlebarDrag::Auto, FLOATING, false, false, 1),
            Press::Move
        );
        assert_eq!(
            title_bar_press(TitlebarDrag::Auto, FLOATING, false, false, 2),
            Press::Maximize
        );
        assert_eq!(
            title_bar_press(TitlebarDrag::Auto, Decorations::Server, false, false, 1),
            Press::Move,
            "without tiling news the window is taken as floating"
        );
    }

    #[test]
    fn a_tiled_window_is_never_moved() {
        // a move request on a tiling compositor pops the window out of its tile or makes it jump
        let left_half = Decorations::Client {
            tiling: gpui::Tiling {
                left: true,
                top: true,
                bottom: true,
                right: false,
            },
        };
        for clicks in [1, 2] {
            assert_eq!(
                title_bar_press(TitlebarDrag::Auto, left_half, false, false, clicks),
                Press::Nothing
            );
            let one_edge = Decorations::Client {
                tiling: gpui::Tiling {
                    right: true,
                    ..Default::default()
                },
            };
            assert_eq!(
                title_bar_press(TitlebarDrag::Auto, one_edge, false, false, clicks),
                Press::Nothing
            );
        }
    }

    #[test]
    fn maximized_or_fullscreen_windows_are_never_moved() {
        // some compositors report these without tiled edges, or with server decorations that hide the tiling
        for decorations in [FLOATING, Decorations::Server] {
            assert_eq!(
                title_bar_press(TitlebarDrag::Auto, decorations, true, false, 1),
                Press::Nothing
            );
            assert_eq!(
                title_bar_press(TitlebarDrag::Auto, decorations, false, true, 1),
                Press::Nothing
            );
            assert_eq!(
                title_bar_press(TitlebarDrag::Auto, decorations, true, false, 2),
                Press::Nothing
            );
        }
    }

    #[test]
    fn never_turns_dragging_off_for_compositors_that_hide_their_tiling() {
        assert_eq!(
            title_bar_press(TitlebarDrag::Never, FLOATING, false, false, 1),
            Press::Nothing
        );
        assert_eq!(
            title_bar_press(TitlebarDrag::Never, Decorations::Server, false, false, 2),
            Press::Nothing
        );
        assert_eq!(TitlebarDrag::default(), TitlebarDrag::Auto);
    }

    #[test]
    fn the_refresh_interval_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        crate::data::prefs::Prefs::load(path.clone()).set(Refresh::KEY, &10u64);
        let secs = crate::data::prefs::Prefs::load(path).get::<u64>(Refresh::KEY);
        assert_eq!(Refresh::stored(secs), Refresh::CHOICES[2]);
        assert_eq!(
            Refresh::stored(Some(7)),
            Refresh::CHOICES[0],
            "a value edited by hand to something not offered falls back"
        );
    }
}
