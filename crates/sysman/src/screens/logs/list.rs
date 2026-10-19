use std::ops::Range;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Div, Hsla, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px, uniform_list,
};
use gpui_component::button::Button;
use gpui_component::scroll::ScrollableElement;
use gpui_component::spinner::Spinner;
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;

use super::model::{self, Item};
use super::{Logs, line_key, scan_failed};
use crate::data::logs::{BootEnd, BootSummary};
use crate::ui::shell::paused;
use crate::ui::theme::{Theme, rem_px, theme};

const ROW: f32 = 22.;
/// What is on screen while a newer scan reads, see [`Logs::stale`].
pub(super) const STALE: f32 = 0.6;
const FADE: Duration = Duration::from_millis(160);

type Action = Box<dyn Fn(&mut Logs, &mut Context<Logs>)>;
const TIME_WIDTH: f32 = 56.;
const DATED_TIME_WIDTH: f32 = 96.;
const SOURCE_WIDTH: f32 = 120.;

/// Errors and worse red, warnings orange, notices blue; info and debug unmarked so the eye goes to what matters.
pub(super) fn priority_colour(p: u8, t: &Theme) -> Option<Hsla> {
    match p {
        0..=3 => Some(t.red),
        4 => Some(t.orange),
        5 => Some(t.blue),
        _ => None,
    }
}

pub(super) fn priority_label(p: u8) -> &'static str {
    match p {
        0 => "Emergency",
        1 => "Alert",
        2 => "Critical",
        3 => "Error",
        4 => "Warning",
        5 => "Notice",
        6 => "Info",
        _ => "Debug",
    }
}

pub(super) fn priority_mark(p: u8, t: &Theme) -> Div {
    let bar = div().flex_shrink_0().w(px(3.)).h(rem_px(12.)).rounded(px(1.5));
    match priority_colour(p, t) {
        Some(c) => bar.bg(c),
        None => bar,
    }
}

pub(super) fn mono(text: impl Into<SharedString>, t: &Theme) -> Div {
    div()
        .font_family(t.mono.clone())
        .text_size(rem_px(11.))
        .child(text.into())
}

/// Turning, unless live work is paused.
pub(super) fn spinner(cx: &App) -> AnyElement {
    let t = theme(cx);
    if paused(cx) {
        Icon::new(gpui_component::IconName::Loader)
            .small()
            .text_color(t.text_tertiary)
            .into_any_element()
    } else {
        Spinner::new().small().color(t.text_tertiary).into_any_element()
    }
}

/// What one list row needs from the index, read while its lock is held only briefly.
struct Line {
    item: Item,
    key: (u64, u64),
    t_us: u64,
    priority: u8,
    source: SharedString,
}

impl Logs {
    pub(super) fn list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = theme(cx);
        let state = self.scan.state();
        if let Some(e) = scan_failed(&state) {
            return self.empty(
                IconName::TriangleAlert,
                "The journal could not be read".into(),
                e.to_string(),
                Some((
                    "Try again",
                    Box::new(|this: &mut Self, cx: &mut Context<Self>| this.retry(cx)),
                )),
                cx,
            );
        }
        let items = self.view.items.clone();
        if items.is_empty() || self.first {
            if !self.view.key.settled || self.first {
                return div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .gap(rem_px(8.))
                    .text_size(t.caption)
                    .text_color(t.text_secondary)
                    .child(spinner(cx))
                    .child("Reading the journal…")
                    .into_any_element();
            }
            let filtered = !self.query.is_empty();
            let no_journal = self.status.is_some_and(|s| s.no_journal);
            return self.empty(
                IconName::Inbox,
                if filtered {
                    "No entries match".into()
                } else if no_journal {
                    "No systemd journal found on this system".into()
                } else {
                    format!("Nothing was logged in {}", self.scope_name())
                },
                if filtered {
                    "Try a wider range, a lower priority or fewer filters.".into()
                } else {
                    String::new()
                },
                filtered.then(|| {
                    (
                        "Clear filters",
                        Box::new(|this: &mut Self, cx: &mut Context<Self>| this.clear_filters(cx)) as Action,
                    )
                }),
                cx,
            );
        }
        // the list is laid out only after its first frame, so keep asking until it is at the end
        if self.stick {
            self.list.scroll_to_bottom();
            if self.view.scanned && self.list.is_scrolled_to_end() == Some(true) {
                self.stick = false;
            }
        }
        let dated = !model::same_day(self.view.range.0, self.view.range.1, &self.tz);
        let list = uniform_list(
            "logs-list",
            items.len(),
            cx.processor(move |this, range: Range<usize>, window, cx| this.rows(&items, range, dated, window, cx)),
        )
        .track_scroll(&self.list)
        .size_full()
        .pt(rem_px(2.))
        .pb(rem_px(8.));
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .text_size(t.caption)
            .font_features(t.numbers.clone())
            .when(self.stale(), |d| d.opacity(STALE))
            .child(list)
            .vertical_scrollbar(&self.list)
            .into_any_element()
    }

    fn clear_filters(&mut self, cx: &mut Context<Self>) {
        self.controls = Default::default();
        self.parsed = Default::default();
        self.text.clear();
        self.clear_input = true;
        self.refilter(cx);
    }

    fn rows(
        &mut self,
        items: &[Item],
        range: Range<usize>,
        dated: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let lines: Vec<Line> = {
            let index = self.scan.index();
            items[range.start.min(items.len())..range.end.min(items.len())]
                .iter()
                .map(|&item| {
                    let row = match item {
                        Item::Entry(v) => index.row(v.row).copied(),
                        Item::BootEnd(_) => None,
                    };
                    Line {
                        item,
                        key: row.as_ref().map_or((0, 0), line_key),
                        t_us: row.map_or(0, |r| r.t_us),
                        priority: row.map_or(6, |r| r.priority),
                        source: row.map_or_else(SharedString::default, |r| index.name(r.source).to_string().into()),
                    }
                })
                .collect()
        };
        let wanted = lines
            .iter()
            .filter_map(|l| match l.item {
                Item::Entry(v) => Some((v.row, l.key)),
                Item::BootEnd(_) => None,
            })
            .collect();
        self.want_lines(wanted, cx);
        let fading = lines
            .iter()
            .any(|l| self.lines.get(&l.key).is_some_and(|(_, at)| at.elapsed() < FADE));
        if fading && !paused(cx) {
            window.request_animation_frame();
        }
        lines
            .into_iter()
            .enumerate()
            .map(|(i, line)| match line.item {
                Item::Entry(_) => self.entry_row(range.start + i, line, dated, cx),
                Item::BootEnd(b) => self.boot_divider(b, cx),
            })
            .collect()
    }

    fn entry_row(&self, ix: usize, line: Line, dated: bool, cx: &Context<Self>) -> AnyElement {
        let t = theme(cx);
        let Item::Entry(v) = line.item else {
            unreachable!("called for entries")
        };
        let selected = self.selected == Some(v.row);
        let message = self.lines.get(&line.key).map(|(m, at)| {
            let shown = if paused(cx) {
                1.
            } else {
                (at.elapsed().as_secs_f32() / FADE.as_secs_f32()).min(1.)
            };
            (m.clone(), shown)
        });
        let row = v.row;
        let time = model::clock(line.t_us, if dated { "%b %-d %H:%M:%S" } else { "%H:%M:%S" }, &self.tz);
        div()
            .id(("logs-row", ix))
            .w_full()
            .h(rem_px(ROW))
            .flex()
            .flex_row()
            .items_center()
            .gap(rem_px(10.))
            .px(rem_px(14.))
            .cursor_pointer()
            .when(selected, |d| d.bg(t.row_selected))
            .when(!selected, |d| d.hover(|d| d.bg(t.card)))
            .on_click(cx.listener(move |this, _, _, cx| this.select(Some(row), cx)))
            .child(priority_mark(line.priority, t))
            .child(
                div()
                    .flex_shrink_0()
                    .w(rem_px(if dated { DATED_TIME_WIDTH } else { TIME_WIDTH }))
                    .text_color(t.text_secondary)
                    .child(time),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .w(rem_px(SOURCE_WIDTH))
                    .truncate()
                    .text_color(t.text_secondary)
                    .child(line.source),
            )
            .child(match message {
                Some((m, shown)) => {
                    mono(m, t)
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .opacity(shown)
                        .text_color(if line.priority >= 6 {
                            t.text.opacity(0.72)
                        } else {
                            t.text
                        })
                }
                None => div().flex_1().min_w_0(),
            })
            .when(v.count > 1, |d| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .px(rem_px(5.))
                        .rounded_full()
                        .bg(t.card_hover)
                        .text_size(t.small_caption)
                        .font_weight(t.weight_medium)
                        .text_color(t.text_secondary)
                        .child(format!("×{}", v.count)),
                )
            })
            .into_any_element()
    }

    /// The end of a finished boot: how it ended, in red when it never shut down.
    fn boot_divider(&self, b: usize, cx: &Context<Self>) -> AnyElement {
        let t = theme(cx);
        let boots: &[BootSummary] = &self.view.boots;
        let Some(boot) = boots.get(b) else {
            return div().h(rem_px(ROW)).into_any_element();
        };
        let name = model::boot_label(b, boots);
        let at = model::clock(boot.last_us, "%b %-d %H:%M:%S", &self.tz);
        let mut text = match boot.end {
            BootEnd::Abrupt => format!("{name} ended without shutdown at {at}"),
            BootEnd::Clean => format!("{name} shut down at {at}"),
            _ => format!("{name} ends at {at}"),
        };
        if let Some(note) = boot.note.as_deref().filter(|_| boot.end == BootEnd::Abrupt) {
            text.push_str(&format!(" · {note}"));
        }
        if let Some(next) = boots.get(b + 1) {
            let gap = match boot.end {
                BootEnd::Abrupt => model::next_boot(boot, next),
                _ => model::off_time(boot, next).to_lowercase(),
            };
            text.push_str(&format!(" · {gap}"));
        }
        let abrupt = boot.end == BootEnd::Abrupt;
        let colour = if abrupt { t.red } else { t.text_tertiary };
        let rule = || div().flex_1().h(px(1.)).bg(colour.opacity(0.35));
        div()
            .w_full()
            .h(rem_px(ROW))
            .flex()
            .flex_row()
            .items_center()
            .gap(rem_px(10.))
            .px(rem_px(14.))
            .text_size(t.small_caption)
            .text_color(if abrupt { t.red } else { t.text_secondary })
            .child(rule())
            .when(abrupt, |d| d.child(Icon::new(IconName::TriangleAlert).xsmall()))
            .child(text)
            .child(rule())
            .into_any_element()
    }

    fn empty(
        &self,
        icon: IconName,
        title: String,
        detail: String,
        action: Option<(&'static str, Action)>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let t = theme(cx);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(rem_px(8.))
            .px(rem_px(24.))
            .child(Icon::new(icon).size_6().text_color(t.text_tertiary))
            .child(
                div()
                    .text_size(t.panel_title)
                    .font_weight(t.weight_semibold)
                    .child(title),
            )
            .when(!detail.is_empty(), |d| {
                d.child(
                    div()
                        .max_w(rem_px(520.))
                        .text_center()
                        .text_size(t.caption)
                        .text_color(t.text_secondary)
                        .child(detail),
                )
            })
            .children(action.map(|(label, f)| {
                Button::new("logs-empty-action")
                    .small()
                    .outline()
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| f(this, cx)))
            }))
            .into_any_element()
    }
}
