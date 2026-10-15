use gpui::{
    App, Hsla, InteractiveElement, IntoElement, ParentElement, Pixels, SharedString, StatefulInteractiveElement,
    Styled, div, relative,
};
use gpui_component::tooltip::Tooltip;

use crate::ui::theme::{rem_px, theme};

pub struct CoreBar {
    pub pct: f64,
    pub color: Hsla,
    pub group: SharedString,
}

/// One bar per logical CPU, filled from the bottom.
pub fn core_bars(bars: Vec<CoreBar>, height: Pixels, cx: &App) -> impl IntoElement {
    let track = theme(cx).card_hover;
    div()
        .flex()
        .flex_row()
        .gap(rem_px(2.))
        .h(height)
        .children(bars.into_iter().enumerate().map(move |(i, bar)| {
            let tip: SharedString = format!("CPU {i} · {:.0}% · {}", bar.pct, bar.group).into();
            div()
                .id(("core", i))
                .relative()
                .flex_1()
                .h_full()
                .rounded(rem_px(2.))
                .overflow_hidden()
                .bg(track)
                .child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .w_full()
                        .h(relative((bar.pct / 100.).clamp(0., 1.) as f32))
                        .bg(bar.color),
                )
                .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        }))
}
