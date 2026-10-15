use gpui::{
    App, Hsla, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, div,
    relative,
};
use gpui_component::tooltip::Tooltip;

use crate::charts::format::bytes;
use crate::ui::theme::{rem_px, theme};

pub struct Segment {
    pub name: SharedString,
    pub bytes: f64,
    pub color: Hsla,
}

/// A rounded composition bar over `total`, with a legend row "name / GB · %" per segment.
pub fn stacked_bar(segments: Vec<Segment>, total: f64, cx: &App) -> impl IntoElement {
    let t = theme(cx);
    let share = |b: f64| if total > 0. { b / total } else { 0. };
    let detail = |s: &Segment| format!("{} · {:.0}%", bytes(s.bytes), share(s.bytes) * 100.);
    let bar = div()
        .flex()
        .flex_row()
        .h(rem_px(22.))
        .rounded(rem_px(6.))
        .overflow_hidden()
        .bg(t.card)
        .children(segments.iter().enumerate().map(|(i, s)| {
            let tip: SharedString = format!("{} · {}", s.name, detail(s)).into();
            div()
                .id(("segment", i))
                .h_full()
                .w(relative(share(s.bytes) as f32))
                .bg(s.color)
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        }));
    let legend = div()
        .flex()
        .flex_col()
        .gap(rem_px(8.))
        .children(segments.iter().map(|s| {
            div()
                .flex()
                .flex_row()
                .gap(rem_px(8.))
                .child(div().mt(rem_px(3.)).size(rem_px(10.)).rounded(rem_px(3.)).bg(s.color))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(t.caption)
                                .font_weight(t.weight_medium)
                                .child(s.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(t.small_caption)
                                .text_color(t.text_secondary)
                                .font_features(t.numbers.clone())
                                .child(detail(s)),
                        ),
                )
        }));
    div().flex().flex_col().gap(rem_px(14.)).child(bar).child(legend)
}
