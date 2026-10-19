use gpui::{
    AnyElement, AppContext, Context, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;
use journal::categories::Category;

use super::Logs;
use super::model::{self, SavedFilter, Span, thousands};
use crate::data::logs::{BootEnd, BootSummary};
use crate::ui::theme::{Theme, rem_px, theme};

const RECENT_BOOTS: usize = 5;
const RAIL: f32 = 16.;

fn category_icon(c: Category) -> IconName {
    match c {
        Category::Kernel => IconName::Cpu,
        Category::Hardware => IconName::CircuitBoard,
        Category::Services => IconName::Layers,
        Category::Apps => IconName::AppWindow,
        Category::Scheduled => IconName::Timer,
        Category::SignIns => IconName::ShieldCheck,
        Category::BootPower => IconName::Zap,
        Category::Crashes => IconName::Skull,
        Category::Packages => IconName::Blocks,
    }
}

fn group(title: &'static str, t: &Theme) -> impl IntoElement + use<> {
    div()
        .px(rem_px(8.))
        .pt(rem_px(14.))
        .pb(rem_px(4.))
        .text_size(t.small_caption)
        .font_weight(t.weight_semibold)
        .text_color(t.text_secondary)
        .child(title.to_uppercase())
}

fn item(id: impl Into<SharedString>, icon: IconName, label: SharedString, selected: bool, t: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(gpui::ElementId::Name(id.into()))
        .flex_shrink_0()
        .h(rem_px(24.))
        .flex()
        .flex_row()
        .items_center()
        .gap(rem_px(7.))
        .px(rem_px(8.))
        .rounded(rem_px(6.))
        .text_size(t.caption)
        .cursor_pointer()
        .when(selected, |d| d.bg(t.selection))
        .when(!selected, |d| d.hover(|d| d.bg(t.card)))
        .child(Icon::new(icon).xsmall().text_color(t.text_secondary))
        .child(div().flex_1().min_w_0().truncate().child(label))
}

impl Logs {
    pub(super) fn sources(&self, cx: &Context<Self>) -> impl IntoElement {
        let boots = self.boots(cx);
        let saved = self.saved_filters(cx);
        let t = theme(cx);
        let counts = self.view.counts;
        let count = |n: u64| {
            div()
                .text_size(t.small_caption)
                .text_color(t.text_tertiary)
                .font_features(t.numbers.clone())
                .child(thousands(n))
        };
        let categories = std::iter::once(None).chain(Category::ALL.map(Some)).map(|c| {
            let (label, icon, n) = match c {
                None => ("All sources", IconName::List, counts.all),
                Some(c) => (model::category_label(c), category_icon(c), counts.get(c)),
            };
            item(
                format!("logs-cat-{label}"),
                icon,
                label.into(),
                self.controls.category == c,
                t,
            )
            .child(count(n))
            .on_click(cx.listener(move |this, _, _, cx| this.set_controls(|ctl| ctl.category = c, cx)))
        });
        div()
            .id("logs-sources")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .px(rem_px(8.))
            .pb(rem_px(12.))
            .bg(t.panel.opacity(0.5))
            .border_r_1()
            .border_color(t.hairline)
            .child(group("Sources", t))
            .children(categories)
            .child(group("Boots", t))
            .child(boots)
            .child(group("Saved filters", t))
            .children(saved)
    }

    fn boots(&self, cx: &Context<Self>) -> AnyElement {
        let boots = &self.view.boots;
        if boots.is_empty() {
            let t = theme(cx);
            return div()
                .px(rem_px(8.))
                .text_size(t.small_caption)
                .text_color(t.text_tertiary)
                .child("Reading boots…")
                .into_any_element();
        }
        let shown = if self.boots_all { boots.len() } else { boots.len().min(RECENT_BOOTS) };
        let oldest_shown = boots.len() - shown;
        let mut rows: Vec<AnyElement> = vec![];
        for i in (oldest_shown..boots.len()).rev() {
            rows.push(self.boot(i, cx).into_any_element());
            if i > oldest_shown {
                rows.push(gap_row(&boots[i - 1], &boots[i], theme(cx)).into_any_element());
            }
        }
        let t = theme(cx);
        let column = div()
            .relative()
            .flex()
            .flex_col()
            .gap(rem_px(2.))
            .child(
                div()
                    .absolute()
                    .top(rem_px(14.))
                    .bottom(rem_px(14.))
                    .left(rem_px(8. + RAIL / 2.))
                    .w(px(1.))
                    .bg(t.text_tertiary.opacity(0.3)),
            )
            .children(rows);
        let hidden = boots.len() - shown;
        let toggle = (boots.len() > RECENT_BOOTS).then(|| {
            div()
                .id("logs-boots-more")
                .px(rem_px(8.))
                .pl(rem_px(8. + RAIL + 8.))
                .py(rem_px(4.))
                .text_size(t.small_caption)
                .text_color(t.blue)
                .cursor_pointer()
                .hover(|d| d.underline())
                .child(if self.boots_all {
                    "Show recent boots only".to_string()
                } else if hidden == 1 {
                    "Show 1 earlier boot".to_string()
                } else {
                    format!("Show {hidden} earlier boots")
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.boots_all = !this.boots_all;
                    cx.notify();
                }))
        });
        div().flex().flex_col().child(column).children(toggle).into_any_element()
    }

    fn boot(&self, i: usize, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let boots = &self.view.boots;
        let b = &boots[i];
        let selected = match &self.span {
            Span::ThisBoot => b.current,
            Span::Boot(id) => *id == b.id,
            Span::Last(_) => false,
        };
        let abrupt = b.end == BootEnd::Abrupt;
        let colour = match b.end {
            _ if b.current => t.green,
            BootEnd::Abrupt => t.red,
            _ => t.text_tertiary,
        };
        let hms = |us| model::clock(us, "%H:%M:%S", &self.tz);
        let end = if b.current { "now".to_string() } else { hms(b.last_us) };
        let last = if b.current { crate::data::logs::now_us() } else { b.last_us };
        let (state, state_colour) = match b.end {
            BootEnd::Running => (Some("Running"), t.text_secondary),
            BootEnd::Clean => (Some("Clean shutdown"), t.text_secondary),
            BootEnd::Abrupt => (Some("Ended without shutdown"), t.red),
            BootEnd::Unknown => (None, t.text_secondary),
        };
        let tally = tally(b);
        let target = if b.current { Span::ThisBoot } else { Span::Boot(b.id.clone()) };
        div()
            .id(("logs-boot", i))
            .flex()
            .flex_row()
            .gap(rem_px(8.))
            .px(rem_px(8.))
            .py(rem_px(6.))
            .rounded(t.sub_radius)
            .cursor_pointer()
            .when(abrupt, |d| d.bg(t.red.opacity(0.07)).border_1().border_color(t.red.opacity(0.25)))
            .when(selected && !abrupt, |d| d.bg(t.selection))
            .when(selected && abrupt, |d| d.bg(t.red.opacity(0.14)))
            .when(!selected, |d| d.hover(|d| d.bg(t.card)))
            .on_click(cx.listener(move |this, _, _, cx| this.set_span(target.clone(), cx)))
            .child(
                div()
                    .w(rem_px(RAIL))
                    .flex_shrink_0()
                    .flex()
                    .justify_center()
                    .pt(rem_px(3.))
                    .child(
                        div()
                            .size(rem_px(9.))
                            .rounded_full()
                            .bg(if b.current { colour } else { t.window })
                            .border_2()
                            .border_color(colour),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .text_size(t.small_caption)
                    .text_color(t.text_secondary)
                    .font_features(t.numbers.clone())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(rem_px(6.))
                            .child(
                                div()
                                    .text_size(t.caption)
                                    .font_weight(t.weight_semibold)
                                    .text_color(t.text)
                                    .child(model::boot_label(i, boots)),
                            )
                            .child(div().flex_1())
                            .child(model::clock(b.first_us, "%b %-d", &self.tz)),
                    )
                    .child(
                        div().truncate().child(format!(
                            "{} → {end} · {}",
                            hms(b.first_us),
                            model::span(last.saturating_sub(b.first_us))
                        )),
                    )
                    .children(state.map(|s| {
                        div()
                            .text_color(state_colour)
                            .when(abrupt, |d| d.font_weight(t.weight_medium))
                            .child(s)
                    }))
                    .children(tally.map(|s| div().truncate().child(s))),
            )
    }

    fn saved_filters(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let t = theme(cx);
        let this = cx.entity().downgrade();
        let mut out: Vec<AnyElement> = self
            .saved
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let SavedFilter { name, query } = f.clone();
                let active = self.text.trim() == query.trim();
                let (menu, chosen) = (this.clone(), query.clone());
                item(format!("logs-saved-{i}"), IconName::ListFilter, name.into(), active, t)
                    .group("logs-saved")
                    .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(query.clone()).build(window, cx))
                    .on_click(cx.listener(move |this, _, window, cx| this.use_filter(chosen.clone(), window, cx)))
                    .child(
                        div()
                            .id(("logs-saved-delete", i))
                            .flex_shrink_0()
                            .p(rem_px(2.))
                            .rounded(rem_px(3.))
                            .text_color(t.text_tertiary)
                            .opacity(0.)
                            .group_hover("logs-saved", |d| d.opacity(1.))
                            .hover(|d| d.bg(t.card_hover).text_color(t.text))
                            .child(Icon::new(IconName::X).xsmall())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.delete_filter(i, cx);
                            })),
                    )
                    .context_menu(move |m, _, _| {
                        let menu = menu.clone();
                        m.item(PopupMenuItem::new("Delete").on_click(move |_, _, cx| {
                            menu.update(cx, |this, cx| this.delete_filter(i, cx)).ok();
                        }))
                    })
                    .into_any_element()
            })
            .collect();
        match &self.naming {
            Some(input) => out.push(
                div()
                    .h(rem_px(26.))
                    .flex()
                    .items_center()
                    .px(rem_px(4.))
                    .rounded(rem_px(6.))
                    .border_1()
                    .border_color(t.blue.opacity(0.6))
                    .bg(t.window)
                    .child(Input::new(input).appearance(false).xsmall())
                    .into_any_element(),
            ),
            None => {
                let can = !self.text.trim().is_empty();
                out.push(
                    item("logs-save-current", IconName::Plus, "Save current search".into(), false, t)
                        .text_color(if can { t.text_secondary } else { t.text_tertiary })
                        .when(!can, |d| d.cursor_default().opacity(0.6))
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(if can {
                                "Keep what the search field holds, under a name"
                            } else {
                                "Type a search first"
                            })
                            .build(window, cx)
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if can {
                                this.start_naming(window, cx);
                            }
                        }))
                        .into_any_element(),
                );
            }
        }
        out
    }

    fn start_naming(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text.trim().to_string();
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Name").default_value(text));
        cx.subscribe(&input, |this, input, event: &InputEvent, cx| match event {
            InputEvent::PressEnter { .. } => {
                let name = input.read(cx).value().trim().to_string();
                if !name.is_empty() {
                    this.saved.push(SavedFilter {
                        name,
                        query: this.text.trim().to_string(),
                    });
                    this.save_filters();
                }
                this.naming = None;
                cx.notify();
            }
            InputEvent::Blur => {
                this.naming = None;
                cx.notify();
            }
            _ => {}
        })
        .detach();
        input.update(cx, |s, cx| s.focus(window, cx));
        self.naming = Some(input);
        cx.notify();
    }

    fn delete_filter(&mut self, i: usize, cx: &mut Context<Self>) {
        if i < self.saved.len() {
            self.saved.remove(i);
            self.save_filters();
            cx.notify();
        }
    }
}

fn tally(b: &BootSummary) -> Option<String> {
    if matches!(b.end, BootEnd::Unknown) {
        return None;
    }
    let parts: Vec<String> = [
        (b.crashes, "crash", "crashes"),
        (b.failed_units, "failed unit", "failed units"),
        (b.oom_kills, "OOM kill", "OOM kills"),
    ]
    .into_iter()
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, one, many)| format!("{n} {}", if n == 1 { one } else { many }))
    .collect();
    Some(if parts.is_empty() {
        "No problems found".into()
    } else {
        parts.join(" · ")
    })
}

fn gap_row(older: &BootSummary, newer: &BootSummary, t: &Theme) -> impl IntoElement + use<> {
    let lost = older.end == BootEnd::Abrupt;
    let colour = if lost { t.red } else { t.text_tertiary };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(rem_px(8.))
        .px(rem_px(8.))
        .py(px(1.))
        .child(
            div()
                .w(rem_px(RAIL))
                .flex_shrink_0()
                .flex()
                .justify_center()
                .child(Icon::new(if lost { IconName::Zap } else { IconName::Pause }).xsmall().text_color(colour)),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(t.small_caption)
                .text_color(colour)
                .font_features(t.numbers.clone())
                .child(model::off_time(older, newer)),
        )
}
