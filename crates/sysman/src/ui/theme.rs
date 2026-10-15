use std::sync::Arc;

use gpui::{App, FontFeatures, Global, Hsla, Pixels, SharedString, px, rgb};

/// Every visual token in one place, so a light variant is another constructor.
pub struct Theme {
    pub window: Hsla,
    pub panel: Hsla,
    pub card: Hsla,
    pub card_hover: Hsla,
    pub hairline: Hsla,
    pub hairline_width: Pixels,
    pub tooltip: Hsla,
    pub text: Hsla,
    pub text_secondary: Hsla,
    pub text_tertiary: Hsla,

    pub green: Hsla,
    pub orange: Hsla,
    pub red: Hsla,
    pub blue: Hsla,
    pub teal: Hsla,
    pub purple: Hsla,
    pub pink: Hsla,
    pub indigo: Hsla,

    pub font: SharedString,
    pub mono: SharedString,
    /// Tabular digits, for every number so columns and live values do not jitter.
    pub numbers: FontFeatures,
    pub panel_title: Pixels,
    pub card_label: Pixels,
    pub card_value: Pixels,
    pub caption: Pixels,
    pub small_caption: Pixels,
    pub axis: Pixels,

    pub page_padding: Pixels,
    pub gap: Pixels,
    pub panel_padding: Pixels,
    pub card_padding_y: Pixels,
    pub card_padding_x: Pixels,
    pub rail_width: Pixels,

    pub panel_radius: Pixels,
    pub card_radius: Pixels,
    pub sub_radius: Pixels,
}

impl Global for Theme {}

fn white(alpha: f32) -> Hsla {
    gpui::white().opacity(alpha)
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            window: rgb(0x1E1E20).into(),
            panel: white(0.05),
            card: white(0.05),
            card_hover: white(0.08),
            hairline: white(0.08),
            hairline_width: px(0.5),
            tooltip: rgb(0x2C2C2F).into(),
            text: white(0.92),
            text_secondary: white(0.60),
            text_tertiary: white(0.38),

            green: rgb(0x30D158).into(),
            orange: rgb(0xFF9F0A).into(),
            red: rgb(0xFF453A).into(),
            blue: rgb(0x0A84FF).into(),
            teal: rgb(0x40C8E0).into(),
            purple: rgb(0xBF5AF2).into(),
            pink: rgb(0xFF375F).into(),
            indigo: rgb(0x5E5CE6).into(),

            font: "Adwaita Sans".into(),
            mono: "Adwaita Mono".into(),
            numbers: FontFeatures(Arc::new(vec![("tnum".into(), 1)])),
            panel_title: px(13.),
            card_label: px(10.),
            card_value: px(15.),
            caption: px(11.),
            small_caption: px(10.),
            axis: px(9.),

            page_padding: px(20.),
            gap: px(16.),
            panel_padding: px(16.),
            card_padding_y: px(11.),
            card_padding_x: px(13.),
            rail_width: px(300.),

            panel_radius: px(12.),
            card_radius: px(10.),
            sub_radius: px(8.),
        }
    }
}

pub fn theme(cx: &App) -> &Theme {
    cx.global::<Theme>()
}
