use gpui::{App, IntoElement, ParentElement, SharedString, Styled, div};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;

use crate::ui::settings;
use crate::ui::theme::{rem_px, theme};

/// One line saying the privileged helper would provide this, and a button that opens Settings.
pub fn helper_hint<T: Into<SharedString>>(id: &'static str, text: T, cx: &App) -> impl IntoElement + use<T> {
    let t = theme(cx);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(rem_px(7.))
        .text_size(t.small_caption)
        .text_color(t.text_secondary)
        .child(
            Icon::new(IconName::Lock)
                .xsmall()
                .text_color(t.text_tertiary)
                .flex_shrink_0(),
        )
        .child(div().flex_1().min_w_0().child(text.into()))
        .child(
            Button::new(id)
                .ghost()
                .xsmall()
                .label("Enable…")
                .on_click(|_, _, cx| settings::open(cx)),
        )
}
