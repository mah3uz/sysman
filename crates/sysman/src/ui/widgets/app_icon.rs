use gpui::{AnyElement, App, IntoElement, Styled, StyledImage, img, px};
use gpui_component::Icon;
use gpui_kit_assets::IconName;

use crate::data::desktop::IconIndex;
use crate::ui::theme::theme;

/// An app icon from its `.desktop` file, else a generic glyph; kernel threads have no command line.
pub fn app_icon(comm: &str, cmdline: &str, icons: &IconIndex, size: f32, cx: &App) -> AnyElement {
    let glyph = if cmdline.is_empty() {
        IconName::Cpu
    } else {
        IconName::AppWindow
    };
    let color = theme(cx).text_tertiary;
    let generic = move || {
        Icon::new(glyph)
            .size(px(size))
            .text_color(color)
            .flex_shrink_0()
            .into_any_element()
    };
    match icons.lookup(comm, cmdline) {
        Some(path) => img(path.clone())
            .size(px(size))
            .flex_shrink_0()
            .with_fallback(generic)
            .into_any_element(),
        None => generic(),
    }
}
