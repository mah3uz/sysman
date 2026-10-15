use gpui::{App, Div, FontWeight, IntoElement, ParentElement, SharedString, Styled, div, px};
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;

use crate::ui::theme::theme;
use crate::ui::widgets::hairline;

/// A titled panel; callers add the body as children.
pub fn panel(icon: IconName, title: impl Into<SharedString>, cx: &App) -> Div {
    let t = theme(cx);
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
            .flex()
            .flex_row()
            .items_center()
            .gap(px(7.))
            .child(Icon::new(icon).small().text_color(t.text_secondary))
            .child(
                div()
                    .text_size(t.panel_title)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title.into()),
            )
            .child(div().flex_1())
            .child(Icon::new(IconName::Maximize2).xsmall().text_color(t.text_tertiary)),
    )
}

/// Uppercase caption label, as on cards and live readouts.
pub fn caption_label(text: &str, cx: &App) -> impl IntoElement + use<> {
    let t = theme(cx);
    div()
        .text_size(t.card_label)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.text_secondary)
        .child(text.to_uppercase())
}
