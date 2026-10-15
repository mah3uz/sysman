use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Context, Entity, FontWeight, Global, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Styled, TitlebarOptions,
    Window, WindowBounds, WindowControlArea, WindowDecorations, WindowOptions, div, prelude::FluentBuilder, px, size,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::spinner::Spinner;
use gpui_component::switch::Switch;
use gpui_component::{Disableable, Icon, Root, Sizable};
use gpui_kit_assets::IconName;

use crate::data::helper::{self, Runner, State, Unit};
use crate::data::live::Live;
use crate::ui::theme::theme;
use crate::ui::widgets::hairline;
use crate::ui::widgets::panel::caption_label;

const TITLE: &str = "sysman Settings";
const TITLE_BAR_HEIGHT: Pixels = px(38.);
const SIDEBAR_WIDTH: Pixels = px(168.);
/// systemd's view is re-read this often while the window is open, to follow changes made elsewhere.
const POLL: Duration = Duration::from_secs(5);
const DESCRIPTION: &str = "Some readings need privileges sysmand does not have: drive health (SMART) needs raw \
                           drive commands, CPU package power is readable only by root, disk I/O and open files of \
                           other users' processes need root, and per-app network traffic needs an eBPF program in \
                           the kernel. The optional sysman-helper reads only these. It runs as a confined system \
                           service, is off by default, and only administrators can read its data.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    PrivilegedData,
}

impl Section {
    const ALL: [Section; 1] = [Self::PrivilegedData];

    fn label(self) -> &'static str {
        match self {
            Self::PrivilegedData => "Privileged data",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Self::PrivilegedData => IconName::ShieldCheck,
        }
    }
}

/// What opening Settings needs from anywhere in the app, and the window once open.
struct Host {
    live: Entity<Live>,
    runner: Arc<dyn Runner>,
    window: Option<AnyWindowHandle>,
}

impl Global for Host {}

pub fn install(live: Entity<Live>, runner: Arc<dyn Runner>, cx: &mut App) {
    cx.set_global(Host {
        live,
        runner,
        window: None,
    });
}

/// Focuses the Settings window, opening it first if it is not open.
pub fn open(cx: &mut App) {
    let Some(host) = cx.try_global::<Host>() else {
        return;
    };
    let (live, runner, window) = (host.live.clone(), host.runner.clone(), host.window);
    if let Some(handle) = window
        && cx
            .update_window(handle, |_, window, _| window.activate_window())
            .is_ok()
    {
        return;
    }
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(640.), px(560.)),
            cx,
        ))),
        window_min_size: Some(size(px(520.), px(420.))),
        titlebar: Some(TitlebarOptions {
            title: Some(TITLE.into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        app_id: Some("sysman".into()),
        window_decorations: Some(WindowDecorations::Client),
        ..Default::default()
    };
    let opened = cx.open_window(options, |window, cx| {
        window.set_window_title(TITLE);
        let view = cx.new(|cx| Settings::new(live, runner, cx));
        cx.new(|cx| Root::new(view, window, cx))
    });
    match opened {
        Ok(handle) => cx.global_mut::<Host>().window = Some(handle.into()),
        Err(e) => eprintln!("sysman: could not open Settings: {e:#}"),
    }
}

pub struct Settings {
    live: Entity<Live>,
    runner: Arc<dyn Runner>,
    section: Section,
    unit: Option<Unit>,
    /// The position the toggle was asked to take, while systemctl runs.
    pending: Option<bool>,
    error: Option<SharedString>,
}

impl Settings {
    fn new(live: Entity<Live>, runner: Arc<dyn Runner>, cx: &mut Context<Self>) -> Self {
        cx.observe(&live, |_, _, cx| cx.notify()).detach();
        cx.spawn(async move |this, cx| {
            loop {
                if this.update(cx, |this, cx| this.query(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(POLL).await;
            }
        })
        .detach();
        Self {
            live,
            runner,
            section: Section::PrivilegedData,
            unit: None,
            pending: None,
            error: None,
        }
    }

    fn query(&mut self, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        let runner = self.runner.clone();
        let task = cx
            .background_executor()
            .spawn(async move { helper::query(runner.as_ref()) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(unit) => this.unit = Some(unit),
                    Err(e) => this.error = Some(e.into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        self.pending = Some(on);
        self.error = None;
        cx.notify();
        let runner = self.runner.clone();
        // Blocks until polkit's dialog is answered, so it must not run on the UI thread.
        let task = cx
            .background_executor()
            .spawn(async move { helper::set(runner.as_ref(), on) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.pending = None;
                if let Err(e) = result {
                    this.error = Some(e.into());
                }
                this.query(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn title_bar(&self, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        div()
            .flex_shrink_0()
            .h(TITLE_BAR_HEIGHT)
            .flex()
            .flex_row()
            .items_center()
            .pr_2()
            .border_b_1()
            .border_color(t.hairline)
            .child(
                div()
                    .id("settings-title")
                    .flex_1()
                    .h_full()
                    .pl(px(36.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .window_control_area(WindowControlArea::Drag)
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                    .child(TITLE),
            )
            .child(
                Button::new("settings-close")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::X))
                    .tooltip("Close")
                    .on_click(|_, window, _| window.remove_window()),
            )
    }

    fn sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        div()
            .flex_shrink_0()
            .w(SIDEBAR_WIDTH)
            .h_full()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p(px(10.))
            .border_r_1()
            .border_color(t.hairline)
            .children(Section::ALL.iter().enumerate().map(|(i, &section)| {
                let selected = section == self.section;
                div()
                    .id(("section", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(6.))
                    .rounded(px(6.))
                    .text_size(px(13.))
                    .when(selected, |d| d.bg(t.card_hover).text_color(t.text))
                    .when(!selected, |d| {
                        d.text_color(t.text_secondary).hover(|d| d.text_color(t.text))
                    })
                    .child(Icon::new(section.icon()).small())
                    .child(section.label())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.section = section;
                        cx.notify();
                    }))
            }))
    }

    fn privileged_data(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let latest = self.live.read(cx).latest.as_ref();
        let module = latest.and_then(|s| s.helper.clone());
        let state = helper::state(module.as_ref(), self.unit.as_ref());
        let tint = match &state {
            State::Running => t.green,
            State::Starting => t.orange,
            State::Error(_) => t.red,
            State::Off | State::NotInstalled => t.text_tertiary,
        };
        let version = module.as_ref().and_then(|m| m.version.clone());
        let status = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.))
            .text_size(t.caption)
            .child(div().size(px(7.)).rounded_full().bg(tint))
            .child(div().font_weight(FontWeight::SEMIBOLD).child(state.label()))
            .when_some(version.filter(|_| state == State::Running), |d, v| {
                d.child(div().text_color(t.text_tertiary).child(format!("version {v}")))
            });
        let detail = match &state {
            State::Error(e) => Some((e.clone(), t.red)),
            State::Starting => Some((
                "The service is active; sysmand looks for it every 10 s.".to_owned(),
                t.text_secondary,
            )),
            State::NotInstalled => Some((
                format!("{} is not installed on this system.", helper::UNIT),
                t.text_secondary,
            )),
            State::Off | State::Running => None,
        };
        let features = helper::features(module.as_ref());
        let feature_list = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .children(features.into_iter().map(|(label, works)| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .text_size(t.caption)
                    .child(
                        Icon::new(if works { IconName::Check } else { IconName::X })
                            .small()
                            .text_color(if works { t.green } else { t.text_tertiary }),
                    )
                    .child(
                        div()
                            .text_color(if works { t.text } else { t.text_secondary })
                            .child(label),
                    )
            }));

        let installed = self.unit.as_ref().is_some_and(|u| u.installed);
        let checked = self.pending.unwrap_or(self.unit.as_ref().is_some_and(Unit::on));
        let toggle_label = match (&self.unit, self.pending) {
            (_, Some(true)) => "Turning on…",
            (_, Some(false)) => "Turning off…",
            (None, None) => "Checking…",
            (Some(u), None) if !u.installed => "Not installed",
            (Some(u), None) if u.on() => "On",
            (Some(_), None) => "Off",
        };
        let toggle = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Privileged helper"),
                    )
                    .child(
                        div()
                            .text_size(t.small_caption)
                            .text_color(t.text_secondary)
                            .child("Turning it on or off asks for an administrator's password."),
                    ),
            )
            .when(self.pending.is_some(), |d| d.child(Spinner::new().small()))
            .child(
                div()
                    .text_size(t.caption)
                    .text_color(t.text_secondary)
                    .child(toggle_label),
            )
            .child(
                Switch::new("helper-toggle")
                    .checked(checked)
                    .disabled(!installed || self.pending.is_some())
                    .on_click(cx.listener(|this, on: &bool, _, cx| this.toggle(*on, cx))),
            );

        let rule = || div().h(t.hairline_width).bg(t.hairline);
        hairline(
            div()
                .flex()
                .flex_col()
                .gap(px(14.))
                .p(t.panel_padding)
                .rounded(t.panel_radius)
                .bg(t.panel)
                .border_color(t.hairline),
            t.hairline_width,
        )
        .child(
            div()
                .text_size(t.caption)
                .text_color(t.text_secondary)
                .line_height(px(17.))
                .child(DESCRIPTION),
        )
        .child(rule())
        .child(toggle)
        .when_some(self.error.clone(), |d, e| {
            d.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(7.))
                    .px(px(10.))
                    .py(px(7.))
                    .rounded(t.sub_radius)
                    .bg(t.red.opacity(0.12))
                    .text_size(t.caption)
                    .text_color(t.red)
                    .child(Icon::new(IconName::TriangleAlert).small().flex_shrink_0())
                    .child(div().min_w_0().child(e)),
            )
        })
        .child(rule())
        .child(caption_label("Status", cx))
        .child(status)
        .when_some(detail, |d, (text, color)| {
            d.child(div().text_size(t.caption).text_color(color).child(text))
        })
        .child(caption_label("Features", cx))
        .child(feature_list)
    }
}

impl Render for Settings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let (window, text, font) = (t.window, t.text, t.font.clone());
        let page = match self.section {
            Section::PrivilegedData => self.privileged_data(cx).into_any_element(),
        };
        let title = self.section.label();
        let t = theme(cx);
        let content = div()
            .id("settings-content")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .p(t.page_padding)
                    .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(page),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(window)
            .text_color(text)
            .font_family(font)
            .child(self.title_bar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(self.sidebar(cx))
                    .child(content),
            )
    }
}
