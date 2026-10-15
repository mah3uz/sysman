use gpui::{
    App, ClickEvent, Div, ElementId, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_component::tooltip::Tooltip;
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;

use crate::ui::theme::theme;
use crate::ui::widgets::hairline;

/// A titled panel whose expand arrow opens a larger view of its chart; callers add the body as children.
pub fn panel(
    icon: IconName,
    title: impl Into<SharedString>,
    on_expand: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let t = theme(cx);
    let title = title.into();
    let id = ElementId::Name(format!("expand-{title}").into());
    frame(cx).child(
        heading(icon, title, cx).child(div().flex_1()).child(
            div()
                .id(id)
                .p(px(3.))
                .rounded(px(4.))
                .text_color(t.text_tertiary)
                .hover(|d| d.bg(t.card_hover).text_color(t.text))
                .child(Icon::new(IconName::Maximize2).xsmall())
                .tooltip(|window, cx| {
                    Tooltip::new("Opens a larger view with statistics and explanations").build(window, cx)
                })
                .on_click(on_expand),
        ),
    )
}

/// A titled panel with nothing to expand.
pub fn section(icon: IconName, title: impl Into<SharedString>, cx: &App) -> Div {
    frame(cx).child(heading(icon, title.into(), cx))
}

/// A section with a one-line explanation under its title.
pub fn section_with(
    icon: IconName,
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
    cx: &App,
) -> Div {
    let t = theme(cx);
    frame(cx).child(
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(heading(icon, title.into(), cx))
            .child(
                div()
                    .text_size(t.caption)
                    .text_color(t.text_secondary)
                    .child(subtitle.into()),
            ),
    )
}

pub fn frame(cx: &App) -> Div {
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
}

pub fn heading(icon: IconName, title: impl Into<SharedString>, cx: &App) -> Div {
    let t = theme(cx);
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
