use gpui::{
    App, Div, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, SharedString, Stateful, Styled, div, px,
};
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;

use crate::charts::sparkline::Sparkline;
use crate::ui::theme::theme;
use crate::ui::widgets::hairline;
use crate::ui::widgets::panel::caption_label;

pub fn metric_card(
    id: &'static str,
    label: &str,
    color: Hsla,
    value: impl Into<SharedString>,
    spark: Sparkline,
    cx: &App,
) -> impl IntoElement {
    let t = theme(cx);
    card(id, label, color, cx)
        .child(
            div()
                .text_size(t.card_value)
                .font_weight(FontWeight::SEMIBOLD)
                .font_features(t.numbers.clone())
                .child(value.into()),
        )
        .child(spark)
}

/// The card frame and its dotted caption row; callers add the body.
pub fn card(id: &'static str, label: &str, color: Hsla, cx: &App) -> Stateful<Div> {
    let t = theme(cx);
    let hover = (t.card_hover, color.opacity(0.45));
    hairline(
        div()
            .id(id)
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(5.))
            .py(t.card_padding_y)
            .px(t.card_padding_x)
            .rounded(t.card_radius)
            .bg(t.card)
            .border_color(t.hairline),
        t.hairline_width,
    )
    .hover(move |s| s.bg(hover.0).border_color(hover.1))
    .child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.))
            .child(div().flex_shrink_0().size(px(6.)).rounded_full().bg(color))
            .child(div().min_w_0().truncate().child(caption_label(label, cx)))
            .child(div().flex_1())
            .child(Icon::new(IconName::Info).xsmall().text_color(t.text_tertiary)),
    )
}
