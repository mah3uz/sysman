use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext, Context, Div, Entity, FontWeight, Global, Hsla, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::spinner::Spinner;
use gpui_component::switch::Switch;
use gpui_component::{Disableable, Icon, Sizable, WindowExt};
use gpui_kit_assets::IconName;

use crate::data::helper::{self, State};
use crate::data::live::{Conn, Live};
use crate::data::systemd::{self, COLLECTOR, HELPER, Runner, Service, Unit};
use crate::ui::shell::{Refresh, Shell};
use crate::ui::theme::theme;
use crate::ui::widgets::hairline;

const WIDTH: f32 = 520.;
const HEIGHT: f32 = 600.;
/// systemd's view is re-read this often while Settings is open, to follow changes made elsewhere.
const POLL: Duration = Duration::from_secs(5);
const ROW_LABEL: f32 = 13.;
const PRIVILEGED_NOTE: &str = "Drive health needs raw drive commands, CPU package power is readable only by root, \
                               other users' disk I/O and open files need root, and per-app traffic needs an eBPF \
                               program in the kernel. The optional sysman-helper reads only these. It runs as a \
                               confined system service, is off by default, and only administrators can read its \
                               data. Turning it on or off asks for authentication.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    General,
    Advanced,
}

impl Tab {
    const ALL: [Tab; 2] = [Self::General, Self::Advanced];

    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Advanced => "Advanced",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::General => IconName::Settings,
            Self::Advanced => IconName::SlidersHorizontal,
        }
    }
}

/// What opening Settings needs from anywhere in the app, and the dialog once open.
struct Host {
    live: Entity<Live>,
    shell: WeakEntity<Shell>,
    runner: Arc<dyn Runner>,
    open: Option<WeakEntity<Settings>>,
}

impl Global for Host {}

pub fn install(live: Entity<Live>, shell: WeakEntity<Shell>, runner: Arc<dyn Runner>, cx: &mut App) {
    cx.set_global(Host {
        live,
        shell,
        runner,
        open: None,
    });
}

/// Opens Settings as a modal dialog over the main window, unless it is already open.
pub fn open(cx: &mut App) {
    let Some(host) = cx.try_global::<Host>() else {
        return;
    };
    let Some(handle) = cx.active_window().or_else(|| cx.windows().first().copied()) else {
        return;
    };
    let (live, shell, runner) = (host.live.clone(), host.shell.clone(), host.runner.clone());
    // A click handler runs while its window is borrowed, so the window is updated once it returns.
    cx.defer(move |cx| {
        if cx.global::<Host>().open.as_ref().is_some_and(|s| s.upgrade().is_some()) {
            return;
        }
        let opened = handle.update(cx, |_, window, cx| {
            let view = cx.new(|cx| Settings::new(live, shell, runner, cx));
            cx.global_mut::<Host>().open = Some(view.downgrade());
            window.open_dialog(cx, move |dialog, window, cx| {
                let t = theme(cx);
                let view = view.clone();
                let top = ((window.viewport_size().height - px(HEIGHT)) / 2.).max(px(16.));
                dialog
                    .w(px(WIDTH))
                    .h(px(HEIGHT))
                    .margin_top(top)
                    .close_button(false)
                    .p_0()
                    .gap_0()
                    .bg(t.window)
                    .border_color(t.control_border)
                    .rounded(px(12.))
                    .content(move |content, _, _| content.min_h_0().child(view.clone()))
            });
        });
        if let Err(e) = opened {
            eprintln!("sysman: could not open Settings: {e:#}");
        }
    });
}

/// A unit behind a switch: what systemd last said, and a change in flight.
struct Toggle {
    service: Service,
    unit: Option<Unit>,
    /// The position the switch was asked to take, while systemctl runs.
    pending: Option<bool>,
    error: Option<SharedString>,
}

impl Toggle {
    fn new(service: Service) -> Self {
        Self {
            service,
            unit: None,
            pending: None,
            error: None,
        }
    }

    fn checked(&self) -> bool {
        self.pending
            .unwrap_or_else(|| self.unit.as_ref().is_some_and(|u| self.service.switched_on(u)))
    }

    fn usable(&self) -> bool {
        self.pending.is_none() && self.unit.as_ref().is_some_and(|u| u.installed)
    }

    /// Shown left of the switch when its position alone does not say enough.
    fn note(&self) -> Option<&'static str> {
        match (&self.unit, self.pending) {
            (_, Some(true)) => Some("Turning on…"),
            (_, Some(false)) => Some("Turning off…"),
            (None, None) => Some("Checking…"),
            (Some(u), None) if !u.installed => Some("Not installed"),
            _ => None,
        }
    }
}

pub struct Settings {
    live: Entity<Live>,
    shell: WeakEntity<Shell>,
    runner: Arc<dyn Runner>,
    tab: Tab,
    helper: Toggle,
    collector: Toggle,
}

impl Settings {
    fn new(live: Entity<Live>, shell: WeakEntity<Shell>, runner: Arc<dyn Runner>, cx: &mut Context<Self>) -> Self {
        cx.observe(&live, |_, _, cx| cx.notify()).detach();
        if let Some(shell) = shell.upgrade() {
            cx.observe(&shell, |_, _, cx| cx.notify()).detach();
        }
        cx.spawn(async move |this, cx| {
            loop {
                let alive = this.update(cx, |this, cx| {
                    this.query(HELPER, cx);
                    this.query(COLLECTOR, cx);
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(POLL).await;
            }
        })
        .detach();
        #[cfg(debug_assertions)]
        let tab = match std::env::var("SYSMAN_DEV_SETTINGS").as_deref() {
            Ok("Advanced") => Tab::Advanced,
            _ => Tab::General,
        };
        #[cfg(not(debug_assertions))]
        let tab = Tab::General;
        Self {
            live,
            shell,
            runner,
            tab,
            helper: Toggle::new(HELPER),
            collector: Toggle::new(COLLECTOR),
        }
    }

    fn toggle(&mut self, service: Service) -> &mut Toggle {
        if service == HELPER {
            &mut self.helper
        } else {
            &mut self.collector
        }
    }

    fn query(&mut self, service: Service, cx: &mut Context<Self>) {
        if self.toggle(service).pending.is_some() {
            return;
        }
        let runner = self.runner.clone();
        let task = cx
            .background_executor()
            .spawn(async move { systemd::query(runner.as_ref(), service) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                let toggle = this.toggle(service);
                match result {
                    Ok(unit) => toggle.unit = Some(unit),
                    Err(e) => toggle.error = Some(e.into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set(&mut self, service: Service, on: bool, cx: &mut Context<Self>) {
        let toggle = self.toggle(service);
        if toggle.pending.is_some() {
            return;
        }
        toggle.pending = Some(on);
        toggle.error = None;
        cx.notify();
        let runner = self.runner.clone();
        // Blocks until polkit's dialog is answered, so it must not run on the UI thread.
        let task = cx
            .background_executor()
            .spawn(async move { systemd::set(runner.as_ref(), service, on) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                let toggle = this.toggle(service);
                toggle.pending = None;
                if let Err(e) = result {
                    toggle.error = Some(e.into());
                }
                this.query(service, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let tabs = div()
            .flex()
            .flex_row()
            .gap(px(4.))
            .children(Tab::ALL.iter().map(|&tab| {
                let selected = tab == self.tab;
                div()
                    .id(tab.label())
                    .w(px(76.))
                    .py(px(5.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(3.))
                    .rounded(px(7.))
                    .text_size(px(11.))
                    .when(selected, |d| d.bg(t.card_hover).text_color(t.text))
                    .when(!selected, |d| {
                        d.text_color(t.text_secondary).hover(|d| d.text_color(t.text))
                    })
                    .child(Icon::new(tab.icon()).size(px(20.)))
                    .child(tab.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = tab;
                        cx.notify();
                    }))
            }));
        div()
            .flex_shrink_0()
            .h(px(64.))
            .flex()
            .flex_row()
            .items_center()
            .pr(px(10.))
            .child(div().flex_1().ml(px(34.)))
            .child(tabs)
            .child(div().flex_1())
            .child(
                Button::new("settings-close")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::X))
                    .tooltip("Close")
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
    }

    fn switch_row(&self, label: &'static str, service: Service, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        let toggle = if service == HELPER {
            &self.helper
        } else {
            &self.collector
        };
        let control = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.))
            .when(toggle.pending.is_some(), |d| d.child(Spinner::new().small()))
            .when_some(toggle.note(), |d, note| {
                d.child(div().text_size(t.caption).text_color(t.text_secondary).child(note))
            })
            .child(
                Switch::new(label)
                    .color(t.blue)
                    .checked(toggle.checked())
                    .disabled(!toggle.usable())
                    .on_click(cx.listener(move |this, on: &bool, _, cx| this.set(service, *on, cx))),
            );
        row(label, control, cx)
    }

    fn general(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = theme(cx);
        let current = self.shell.upgrade().map(|s| s.read(cx).refresh());
        let shell = self.shell.clone();
        let interval = Button::new("refresh-interval")
            .ghost()
            .small()
            .label(current.map_or("–".into(), Refresh::label))
            .child(Icon::new(IconName::ChevronDown).xsmall())
            .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |mut menu, _, _| {
                for choice in Refresh::CHOICES {
                    let shell = shell.clone();
                    menu = menu.item(
                        PopupMenuItem::new(choice.label())
                            .checked(Some(choice) == current)
                            .on_click(move |_, _, cx| {
                                shell.update(cx, |shell, cx| shell.set_refresh(choice, cx)).ok();
                            }),
                    );
                }
                menu
            });
        let connected = self.live.read(cx).conn == Conn::Connected;
        let status = status_value(
            if connected { "Running" } else { "Stopped" }.into(),
            if connected { t.green } else { t.orange },
            cx,
        );
        let mut collector = vec![
            row("Status", status, cx),
            self.switch_row("Start at login", COLLECTOR, cx),
        ];
        if let Some(e) = self.collector.error.clone() {
            collector.push(error_row(e, cx));
        }
        vec![
            group(vec![row("Refresh interval", interval, cx)], cx).into_any_element(),
            footnote("How often charts redraw. Collection keeps its own interval.", cx).into_any_element(),
            group_title("Collector", cx).into_any_element(),
            group(collector, cx).into_any_element(),
            footnote(
                "sysmand runs as your user service and records history while the app is closed.",
                cx,
            )
            .into_any_element(),
        ]
    }

    fn advanced(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = theme(cx);
        let module = self.live.read(cx).latest.as_ref().and_then(|s| s.helper.clone());
        let state = helper::state(module.as_ref(), self.helper.unit.as_ref());
        let tint = match &state {
            State::Running => t.green,
            State::Starting => t.orange,
            State::Error(_) => t.red,
            State::Off | State::NotInstalled => t.text_tertiary,
        };
        let (green, dim) = (t.green, t.text_tertiary);
        let label = match (&state, module.as_ref().and_then(|m| m.version.as_deref())) {
            (State::Running, Some(v)) => format!("Running · {v}"),
            _ => state.label().to_owned(),
        };
        let mut rows = vec![self.switch_row("Read privileged data", HELPER, cx)];
        if let Some(e) = self.helper.error.clone() {
            rows.push(error_row(e, cx));
        }
        rows.push(row("Status", status_value(label.into(), tint, cx), cx));
        match &state {
            State::Error(e) => rows.push(error_row(e.clone().into(), cx)),
            State::Starting => rows.push(note_row("The service is active; sysmand looks for it every 10 s.", cx)),
            _ => {}
        }
        for (feature, works) in helper::features(module.as_ref()) {
            let mark = Icon::new(if works { IconName::Check } else { IconName::X })
                .small()
                .text_color(if works { green } else { dim });
            rows.push(row(feature, mark, cx));
        }
        vec![
            group_title("Privileged data", cx).into_any_element(),
            group(rows, cx).into_any_element(),
            footnote(PRIVILEGED_NOTE, cx).into_any_element(),
        ]
    }
}

/// An inset group: rows stacked with hairlines between them.
fn group(rows: Vec<AnyElement>, cx: &App) -> Div {
    let t = theme(cx);
    let count = rows.len();
    let rule_colour = t.hairline;
    hairline(
        div()
            .flex()
            .flex_col()
            .rounded(t.card_radius)
            .bg(t.panel)
            .border_color(t.hairline),
        t.hairline_width,
    )
    .children(rows.into_iter().enumerate().flat_map(move |(i, row)| {
        let rule = (i + 1 < count).then(|| div().mx(px(12.)).h(px(1.)).bg(rule_colour).into_any_element());
        std::iter::once(row).chain(rule)
    }))
}

/// Label at the left, control at the right.
fn row(label: impl Into<SharedString>, control: impl IntoElement, cx: &App) -> AnyElement {
    let t = theme(cx);
    div()
        .min_h(px(38.))
        .px(px(12.))
        .py(px(6.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(ROW_LABEL))
                .text_color(t.text)
                .child(label.into()),
        )
        .child(control)
        .into_any_element()
}

fn error_row(text: SharedString, cx: &App) -> AnyElement {
    let t = theme(cx);
    div()
        .px(px(12.))
        .py(px(7.))
        .flex()
        .flex_row()
        .gap(px(7.))
        .text_size(t.caption)
        .text_color(t.red)
        .child(Icon::new(IconName::TriangleAlert).small().flex_shrink_0())
        .child(div().min_w_0().child(text))
        .into_any_element()
}

fn note_row(text: &'static str, cx: &App) -> AnyElement {
    let t = theme(cx);
    div()
        .px(px(12.))
        .py(px(7.))
        .text_size(t.caption)
        .text_color(t.text_secondary)
        .child(text)
        .into_any_element()
}

fn status_value(label: SharedString, tint: Hsla, cx: &App) -> impl IntoElement + use<> {
    let t = theme(cx);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .text_size(px(ROW_LABEL))
        .text_color(t.text_secondary)
        .child(div().size(px(7.)).rounded_full().bg(tint))
        .child(label)
}

fn group_title(text: &'static str, cx: &App) -> impl IntoElement + use<> {
    let t = theme(cx);
    div()
        .px(px(4.))
        .pt(px(6.))
        .text_size(t.caption)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.text_secondary)
        .child(text)
}

fn footnote(text: &'static str, cx: &App) -> impl IntoElement + use<> {
    let t = theme(cx);
    div()
        .px(px(4.))
        .text_size(t.small_caption)
        .line_height(px(14.))
        .text_color(t.text_secondary)
        .child(text)
}

impl Render for Settings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.tab {
            Tab::General => self.general(cx),
            Tab::Advanced => self.advanced(cx),
        };
        let toolbar = self.toolbar(cx).into_any_element();
        let t = theme(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.window)
            .text_color(t.text)
            .font_family(t.font.clone())
            .child(toolbar)
            .child(
                div()
                    .id("settings-content")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .px(t.page_padding)
                            .pt(px(4.))
                            .pb(t.page_padding)
                            .children(body),
                    ),
            )
    }
}
