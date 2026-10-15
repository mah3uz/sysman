use gpui::{App, IntoElement, ParentElement, Styled, div, px};

use crate::ui::theme::theme;

/// "Average (15 s)" line swatch and "Recorded range" band swatch, under every trend chart.
pub fn legend(width_s: i64, cx: &App) -> impl IntoElement {
    let t = theme(cx);
    let step = if width_s < 60 {
        format!("{width_s} s")
    } else if width_s < 3600 {
        format!("{} min", width_s / 60)
    } else {
        format!("{} h", width_s / 3600)
    };
    let item = |swatch: gpui::Div, text: String| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(swatch)
            .child(text)
    };
    div()
        .flex()
        .flex_row()
        .gap(px(16.))
        .text_size(t.small_caption)
        .text_color(t.text_secondary)
        .child(item(
            div().w(px(14.)).h(px(2.)).rounded(px(1.)).bg(t.text_secondary),
            format!("Average ({step})"),
        ))
        .child(item(
            div()
                .w(px(14.))
                .h(px(8.))
                .rounded(px(2.))
                .bg(t.text_secondary.opacity(0.3)),
            "Recorded range".into(),
        ))
}
