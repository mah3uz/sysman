use gpui::{
    AnyElement, Context, Div, ElementId, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, div, prelude::FluentBuilder, px,
};
use gpui_component::Sizable;
use gpui_component::input::Textarea;
use journal::Entry;
use journal::categories::source;

use super::list::{mono, priority_colour, priority_label, priority_mark};
use super::{Inspected, Logs, model};
use crate::ui::theme::{Theme, rem_px, theme};

const KEY_WIDTH: f32 = 140.;

/// The whole text of the entry, or of the transaction behind a package row.
pub(super) fn full_message(i: &Inspected) -> String {
    if let Some(t) = &i.transaction {
        let mut lines: Vec<String> = t.command.iter().map(|c| format!("$ {c}")).collect();
        lines.extend(t.changes.iter().map(|c| match (&c.old, &c.new) {
            (Some(old), Some(new)) => format!("{:?} {} {old} → {new}", c.action, c.name),
            (_, Some(v)) | (Some(v), None) => format!("{:?} {} {v}", c.action, c.name),
            (None, None) => format!("{:?} {}", c.action, c.name),
        }));
        lines.extend(t.warnings.iter().map(|w| format!("warning: {w}")));
        return lines.join("\n");
    }
    match (&i.entry, &i.error) {
        (Some(e), _) => model::plain(e.message().trim()),
        (None, Some(err)) => format!("Could not read this entry: {err}"),
        (None, None) => "This entry is no longer in the journal.".into(),
    }
}

fn heading(text: &'static str, t: &Theme) -> Div {
    div()
        .text_size(t.small_caption)
        .font_weight(t.weight_semibold)
        .text_color(t.text_secondary)
        .child(text.to_uppercase())
}

impl Logs {
    pub(super) fn inspector(&self, cx: &Context<Self>) -> AnyElement {
        let t = theme(cx);
        let frame = div()
            .id("logs-inspector")
            .size_full()
            .overflow_y_scroll()
            .border_l_1()
            .border_t_1()
            .rounded_tl(t.panel_radius)
            .border_color(t.hairline)
            .bg(t.panel);
        let body = div().flex().flex_col().gap(rem_px(16.)).p(rem_px(14.));
        let Some(row) = self.selected else {
            return frame.into_any_element();
        };
        let (t_us, priority, from) = {
            let index = self.scan.index();
            index
                .row(row)
                .map(|r| (r.t_us, r.priority, index.name(r.source).to_string()))
                .unwrap_or_default()
        };
        let inspected = self.inspected.as_ref().filter(|i| i.row == row);
        let colour = priority_colour(priority, t).unwrap_or(t.text_tertiary);
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(rem_px(6.))
            .text_size(t.caption)
            .font_features(t.numbers.clone())
            .child(div().size(rem_px(7.)).rounded_full().bg(colour))
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(colour)
                    .font_weight(t.weight_semibold)
                    .child(priority_label(priority)),
            )
            .child(div().min_w_0().truncate().text_color(t.text_secondary).child(format!("· {from}")))
            .child(div().flex_1())
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(t.text_secondary)
                    .child(model::clock(t_us, "%b %-d %H:%M:%S%.3f", &self.tz)),
            );
        let message = match (inspected, &self.message) {
            (Some(_), Some(state)) => div()
                .font_family(t.mono.clone())
                .child(
                    Textarea::new(state)
                        .appearance(false)
                        .small()
                        .p_0()
                        .text_size(rem_px(12.)),
                )
                .into_any_element(),
            _ => div()
                .flex()
                .flex_col()
                .gap(rem_px(6.))
                .child(div().w_full().h(rem_px(10.)).rounded(rem_px(4.)).bg(t.card))
                .child(div().w(rem_px(180.)).h(rem_px(10.)).rounded(rem_px(4.)).bg(t.card))
                .into_any_element(),
        };
        let around = inspected
            .filter(|i| i.entry.is_some())
            .map(|i| self.around(i, t))
            .map(IntoElement::into_any_element);
        let fields = inspected
            .and_then(|i| i.entry.as_deref())
            .map(|e| fields(e, t).into_any_element());
        frame
            .child(
                body.child(div().flex().flex_col().gap(rem_px(8.)).child(header).child(message))
                    .children(around)
                    .children(fields),
            )
            .into_any_element()
    }

    /// The entries just before and after, unfiltered, with this one in its place.
    fn around(&self, i: &Inspected, t: &Theme) -> impl IntoElement + use<> {
        let line = |id: ElementId, e: &Entry, me: bool| {
            let message: String = model::plain(e.message()).lines().next().unwrap_or("").chars().take(200).collect();
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap(rem_px(7.))
                .h(rem_px(19.))
                .px(rem_px(4.))
                .rounded(rem_px(4.))
                .text_size(t.small_caption)
                .when(me, |d| d.bg(t.row_selected))
                .child(priority_mark(e.priority(), t).h(rem_px(10.)))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(t.text_secondary)
                        .font_features(t.numbers.clone())
                        .child(model::clock(e.t_us, "%H:%M:%S", &self.tz)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(t.mono.clone())
                        .text_color(if e.priority() >= 6 && !me { t.text.opacity(0.72) } else { t.text })
                        .child(message),
                )
                .tooltip({
                    let tip = SharedString::from(format!("{} · {}", source(e), e.message()));
                    move |window, cx| gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                })
        };
        let me = i.entry.as_deref();
        div()
            .id("logs-around")
            .flex()
            .flex_col()
            .gap(px(1.))
            .child(div().pb(rem_px(4.)).child(heading("Around this entry", t)))
            .children(i.around.before.iter().enumerate().map(|(n, e)| line(("before", n).into(), e, false)))
            .children(me.map(|e| line("this".into(), e, true)))
            .children(i.around.after.iter().enumerate().map(|(n, e)| line(("after", n).into(), e, false)))
    }
}

fn fields(e: &Entry, t: &Theme) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_col()
        .gap(rem_px(4.))
        .child(div().pb(rem_px(2.)).child(heading("Fields", t)))
        .children(e.fields.iter().filter(|(k, _)| k.as_str() != "MESSAGE").map(|(k, v)| {
            div()
                .flex()
                .flex_row()
                .gap(rem_px(10.))
                .text_size(t.small_caption)
                .child(
                    mono(k.clone(), t)
                        .text_size(t.small_caption)
                        .w(rem_px(KEY_WIDTH))
                        .flex_shrink_0()
                        .truncate()
                        .text_color(t.text_secondary),
                )
                .child(mono(v.clone(), t).text_size(t.small_caption).flex_1().min_w_0().truncate())
        }))
}
