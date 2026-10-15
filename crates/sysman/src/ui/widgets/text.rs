use gpui::{App, Div, Hsla, ParentElement, SharedString, Styled, div};

use crate::ui::theme::{rem_px, theme};

/// A small secondary line under a chart or value.
pub fn caption(text: impl Into<SharedString>, cx: &App) -> Div {
    let t = theme(cx);
    div()
        .text_size(t.small_caption)
        .text_color(t.text_secondary)
        .child(text.into())
}

/// Explanatory body text, one size up from a caption.
pub fn footnote(text: impl Into<SharedString>, cx: &App) -> Div {
    let t = theme(cx);
    div()
        .text_size(t.caption)
        .text_color(t.text_secondary)
        .child(text.into())
}

/// Label at the left, value at the right.
pub fn key_value(label: impl Into<SharedString>, value: impl Into<SharedString>, color: Hsla, cx: &App) -> Div {
    let t = theme(cx);
    div()
        .flex()
        .flex_row()
        .gap(rem_px(12.))
        .text_size(t.caption)
        .child(div().flex_shrink_0().text_color(t.text_secondary).child(label.into()))
        .child(div().flex_1())
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_color(color)
                .font_features(t.numbers.clone())
                .child(value.into()),
        )
}

/// Label/value rows; a row with no value is left out.
pub fn facts(rows: Vec<(&str, Option<String>, Hsla)>, cx: &App) -> Div {
    div().flex().flex_col().gap(rem_px(7.)).children(
        rows.into_iter()
            .filter_map(|(label, value, color)| Some(key_value(label.to_owned(), value?, color, cx))),
    )
}

/// A thin capsule track filled to `fraction`, for a share of a whole.
pub fn proportion(fraction: f64, color: Hsla, cx: &App) -> Div {
    let t = theme(cx);
    div().w_full().h(rem_px(5.)).rounded_full().bg(t.card_hover).child(
        div()
            .h_full()
            .min_w(rem_px(4.))
            .w(gpui::relative(fraction.clamp(0., 1.) as f32))
            .rounded_full()
            .bg(color),
    )
}
