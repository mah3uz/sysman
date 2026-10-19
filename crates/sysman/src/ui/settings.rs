use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext, Context, Div, Entity, Global, Hsla, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::spinner::Spinner;
use gpui_component::switch::Switch;
use gpui_component::{Disableable, Icon, Sizable, WindowExt};
use gpui_kit_assets::IconName;

use crate::charts::format::TempUnit;
use crate::data::helper::{self, State};
use crate::data::live::{Conn, Live};
use crate::data::prefs::prefs;
use crate::data::systemd::{self, COLLECTOR, HELPER, Runner, Service, Unit};
use crate::ui::shell::{PauseAfter, Refresh, Shell, TitlebarDrag};
use crate::ui::theme::{self, Appearance, TextSize, rem_px, theme};
use crate::ui::widgets::hairline;
use crate::ui::widgets::segmented::segmented_full;

mod collector;

use collector::CollectorForm;

const WIDTH: f32 = 560.;
const HEIGHT: f32 = 640.;
/// systemd's view is re-read this often while Settings is open, to follow changes made elsewhere.
const POLL: Duration = Duration::from_secs(5);
/// Both settings files are checked this often for edits made by hand.
const FILE_POLL: Duration = Duration::from_secs(2);
const ROW_LABEL: f32 = 13.;
const PRIVILEGED_NOTE: &str = "Drive health needs raw drive commands, CPU package power is readable only by root, \
                               other users' disk I/O and open files need root, and per-app traffic needs an eBPF \
                               program in the kernel. The optional sysman-helper reads only these. It runs as a \
                               confined system service, is off by default, and only administrators can read its \
                               data. Turning it on or off asks for authentication.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    General,
    Display,
    Collector,
    Modules,
    Advanced,
}

impl Tab {
    const ALL: [Tab; 5] = [
        Self::General,
        Self::Display,
        Self::Collector,
        Self::Modules,
        Self::Advanced,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Display => "Display",
            Self::Collector => "Collector",
            Self::Modules => "Modules",
            Self::Advanced => "Advanced",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::General => IconName::Settings,
            Self::Display => IconName::Monitor,
            Self::Collector => IconName::Database,
            Self::Modules => IconName::Blocks,
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
                let rem = window.rem_size();
                let height = rem_px(HEIGHT)
                    .to_pixels(rem)
                    .min(window.viewport_size().height - px(32.));
                let top = ((window.viewport_size().height - height) / 2.).max(px(16.));
                dialog
                    .w(rem_px(WIDTH).to_pixels(rem))
                    .h(height)
                    .margin_top(top)
                    .close_button(false)
                    .p_0()
                    .gap_0()
                    .bg(t.window)
                    .border_color(t.control_border)
                    .rounded(rem_px(12.))
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
    form: Entity<CollectorForm>,
    /// "Reset layout" was pressed and waits for its confirmation.
    confirm_reset: bool,
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
        cx.spawn(async move |this, cx| {
            loop {
                if this.update(cx, |this, cx| this.poll_files(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(FILE_POLL).await;
            }
        })
        .detach();
        let form = cx.new(|_| CollectorForm::new(sysman_config::config_path(), runner.clone()));
        cx.observe(&form, |_, _, cx| cx.notify()).detach();
        #[cfg(debug_assertions)]
        let tab = std::env::var("SYSMAN_DEV_SETTINGS")
            .ok()
            .and_then(|name| Tab::ALL.into_iter().find(|t| t.label().eq_ignore_ascii_case(&name)))
            .unwrap_or(Tab::General);
        #[cfg(not(debug_assertions))]
        let tab = Tab::General;
        Self {
            live,
            shell,
            runner,
            tab,
            helper: Toggle::new(HELPER),
            collector: Toggle::new(COLLECTOR),
            form,
            confirm_reset: false,
        }
    }

    /// Takes in edits made by hand to `ui.toml` and the collector's `config.toml`.
    fn poll_files(&mut self, cx: &mut Context<Self>) {
        let changed = prefs().lock().is_ok_and(|mut p| {
            let changed = p.changed_on_disk();
            if changed {
                p.reload();
            }
            changed
        });
        if changed && let Some(shell) = self.shell.upgrade() {
            shell.update(cx, |shell, cx| shell.prefs_changed(cx));
        }
        self.form.update(cx, |form, cx| form.poll(cx));
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
            .gap(rem_px(4.))
            .children(Tab::ALL.iter().map(|&tab| {
                let selected = tab == self.tab;
                div()
                    .id(tab.label())
                    .w(rem_px(76.))
                    .py(rem_px(5.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(rem_px(3.))
                    .rounded(rem_px(7.))
                    .text_size(rem_px(11.))
                    .when(selected, |d| d.bg(t.card_hover).text_color(t.text))
                    .when(!selected, |d| {
                        d.text_color(t.text_secondary).hover(|d| d.text_color(t.text))
                    })
                    .child(Icon::new(tab.icon()).size(rem_px(20.)))
                    .child(tab.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tab = tab;
                        cx.notify();
                    }))
            }));
        div()
            .flex_shrink_0()
            .h(rem_px(64.))
            .flex()
            .flex_row()
            .items_center()
            .pr(rem_px(10.))
            .child(div().flex_1().ml(rem_px(34.)))
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
            .gap(rem_px(8.))
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
        let pause_after = self.shell.upgrade().map(|s| s.read(cx).pause_after());
        let shell = self.shell.clone();
        let pause = Button::new("pause-after")
            .ghost()
            .small()
            .label(pause_after.map_or("–".into(), PauseAfter::label))
            .child(Icon::new(IconName::ChevronDown).xsmall())
            .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |mut menu, _, _| {
                for choice in PauseAfter::CHOICES {
                    let shell = shell.clone();
                    menu = menu.item(
                        PopupMenuItem::new(choice.label())
                            .checked(Some(choice) == pause_after)
                            .on_click(move |_, _, cx| {
                                shell.update(cx, |shell, cx| shell.set_pause_after(choice, cx)).ok();
                            }),
                    );
                }
                menu
            });
        vec![
            group(
                vec![
                    row("Refresh interval", interval, cx),
                    row("Pause when not focused", pause, cx),
                ],
                cx,
            )
            .into_any_element(),
            footnote(
                "How often charts redraw, and when they stop while you work elsewhere. Collection keeps its own \
                 interval, and history is recorded either way.",
                cx,
            )
            .into_any_element(),
        ]
    }

    fn display(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = theme(cx);
        let appearance = segmented_full(
            "appearance",
            Appearance::ALL.map(|a| (a, a.label())),
            &theme::appearance(cx),
            |appearance, _, cx| theme::set_appearance(appearance, cx),
            cx,
        );
        let text_size = segmented_full(
            "text-size",
            TextSize::ALL.map(|s| (s, s.label())),
            &t.text_size,
            |size, _, cx| theme::set_text_size(size, cx),
            cx,
        );
        let unit = segmented_full(
            "temperature-unit",
            TempUnit::ALL.map(|u| (u, u.label())),
            &TempUnit::current(),
            |unit, _, cx| {
                unit.choose();
                cx.refresh_windows();
            },
            cx,
        );
        let shell = self.shell.clone();
        let drag = segmented_full(
            "titlebar-drag",
            TitlebarDrag::ALL.map(|d| (d, d.label())),
            &self
                .shell
                .upgrade()
                .map_or(TitlebarDrag::Auto, |s| s.read(cx).titlebar_drag()),
            move |choice, _, cx| {
                shell.update(cx, |shell, cx| shell.set_titlebar_drag(choice, cx)).ok();
            },
            cx,
        );
        let reset = if self.confirm_reset {
            div()
                .flex()
                .flex_row()
                .gap(rem_px(4.))
                .child(
                    Button::new("reset-cancel")
                        .ghost()
                        .small()
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_reset = false;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("reset-confirm")
                        .danger()
                        .small()
                        .label("Reset")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_reset = false;
                            if let Some(shell) = this.shell.upgrade() {
                                shell.update(cx, |shell, cx| shell.reset_layout(cx));
                            }
                            cx.notify();
                        })),
                )
        } else {
            div().child(
                Button::new("reset-layout")
                    .ghost()
                    .small()
                    .label("Reset…")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.confirm_reset = true;
                        cx.notify();
                    })),
            )
        };
        vec![
            group(
                vec![
                    row("Appearance", div().w(rem_px(210.)).child(appearance), cx),
                    row("Text size", div().w(rem_px(210.)).child(text_size), cx),
                    row("Temperature", div().w(rem_px(110.)).child(unit), cx),
                ],
                cx,
            )
            .into_any_element(),
            footnote(
                "System follows the desktop's light or dark style as it changes. Bold draws text a little larger \
                 and heavier. Bigger scales the whole layout by about 15%.",
                cx,
            )
            .into_any_element(),
            group_title("Window", cx).into_any_element(),
            group(
                vec![
                    row("Drag window from title bar", div().w(rem_px(130.)).child(drag), cx),
                    row(
                        if self.confirm_reset {
                            "Reset sidebar widths and column choices?"
                        } else {
                            "Reset layout"
                        },
                        reset,
                        cx,
                    ),
                ],
                cx,
            )
            .into_any_element(),
            footnote(
                "Auto moves the window from empty title-bar space only while it floats, never while it is tiled, \
                 maximised or fullscreen. Choose Never if your compositor still moves tiled windows.",
                cx,
            )
            .into_any_element(),
        ]
    }

    fn collector(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = theme(cx);
        let connected = self.live.read(cx).conn == Conn::Connected;
        let status = status_value(
            if connected { "Running" } else { "Stopped" }.into(),
            if connected { t.green } else { t.orange },
            cx,
        );
        let mut service = vec![
            row("Status", status, cx),
            self.switch_row("Start at login", COLLECTOR, cx),
        ];
        if let Some(e) = self.collector.error.clone() {
            service.push(error_row(e, cx));
        }
        let mut out = vec![
            group(service, cx).into_any_element(),
            footnote(
                "sysmand runs as your user service and records history while the app is closed.",
                cx,
            )
            .into_any_element(),
        ];
        out.extend(self.form.update(cx, |form, cx| form.collector_tab(window, cx)));
        out
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
        let rule = (i + 1 < count).then(|| div().mx(rem_px(12.)).h(px(1.)).bg(rule_colour).into_any_element());
        std::iter::once(row).chain(rule)
    }))
}

/// Label at the left, control at the right.
fn row(label: impl Into<SharedString>, control: impl IntoElement, cx: &App) -> AnyElement {
    let t = theme(cx);
    div()
        .min_h(rem_px(38.))
        .px(rem_px(12.))
        .py(rem_px(6.))
        .flex()
        .flex_row()
        .items_center()
        .gap(rem_px(12.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(rem_px(ROW_LABEL))
                .text_color(t.text)
                .child(label.into()),
        )
        .child(control)
        .into_any_element()
}

fn error_row(text: SharedString, cx: &App) -> AnyElement {
    let t = theme(cx);
    div()
        .px(rem_px(12.))
        .py(rem_px(7.))
        .flex()
        .flex_row()
        .gap(rem_px(7.))
        .text_size(t.caption)
        .text_color(t.red)
        .child(Icon::new(IconName::TriangleAlert).small().flex_shrink_0())
        .child(div().min_w_0().child(text))
        .into_any_element()
}

fn note_row(text: &'static str, cx: &App) -> AnyElement {
    let t = theme(cx);
    div()
        .px(rem_px(12.))
        .py(rem_px(7.))
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
        .gap(rem_px(6.))
        .text_size(rem_px(ROW_LABEL))
        .text_color(t.text_secondary)
        .child(div().size(rem_px(7.)).rounded_full().bg(tint))
        .child(label)
}

fn group_title(text: &'static str, cx: &App) -> impl IntoElement + use<> {
    let t = theme(cx);
    div()
        .px(rem_px(4.))
        .pt(rem_px(6.))
        .text_size(t.caption)
        .font_weight(t.weight_semibold)
        .text_color(t.text_secondary)
        .child(text)
}

fn footnote(text: &'static str, cx: &App) -> impl IntoElement + use<> {
    let t = theme(cx);
    div()
        .px(rem_px(4.))
        .text_size(t.small_caption)
        .line_height(rem_px(14.))
        .text_color(t.text_secondary)
        .child(text)
}

impl Render for Settings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.tab {
            Tab::General => self.general(cx),
            Tab::Display => self.display(cx),
            Tab::Collector => self.collector(window, cx),
            Tab::Modules => self.form.update(cx, |form, cx| form.modules_tab(window, cx)),
            Tab::Advanced => self.advanced(cx),
        };
        let footer =
            matches!(self.tab, Tab::Collector | Tab::Modules).then(|| self.form.update(cx, |form, cx| form.footer(cx)));
        let toolbar = self.toolbar(cx).into_any_element();
        let t = theme(cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .text_color(t.text)
            .font_family(t.font.clone())
            .font_weight(t.weight_body)
            .child(toolbar)
            .child(
                div()
                    .id(SharedString::from(format!("settings-content-{}", self.tab.label())))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(rem_px(8.))
                            .px(t.page_padding)
                            .pt(rem_px(4.))
                            .pb(t.page_padding)
                            .children(body),
                    ),
            )
            .children(footer)
    }
}
