use std::sync::Arc;

use gpui::{App, FontFeatures, FontWeight, Global, Hsla, Pixels, Rems, SharedString, px, rems, rgb};
use serde::{Deserialize, Serialize};

use crate::data::prefs::prefs;

/// How large and heavy text is drawn; the whole layout scales with it so its proportions hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextSize {
    #[default]
    Regular,
    Bold,
    Bigger,
}

impl TextSize {
    pub const ALL: [TextSize; 3] = [Self::Regular, Self::Bold, Self::Bigger];
    const KEY: &str = "display.text_size";

    pub fn label(self) -> &'static str {
        match self {
            Self::Regular => "Regular",
            Self::Bold => "Bold",
            Self::Bigger => "Bigger",
        }
    }

    /// The window's root rem size, which every theme token and every [`rem_px`] size derives from.
    pub fn rem_size(self) -> Pixels {
        match self {
            Self::Regular => px(16.),
            // about one pixel more at the 11–13 px most text uses
            Self::Bold => px(17.25),
            Self::Bigger => px(18.4),
        }
    }

    /// Added to every font weight.
    fn weight_step(self) -> f32 {
        match self {
            Self::Bold => 100.,
            Self::Regular | Self::Bigger => 0.,
        }
    }

    pub fn load() -> Self {
        prefs().lock().ok().and_then(|p| p.get(Self::KEY)).unwrap_or_default()
    }

    fn save(self) {
        if let Ok(mut p) = prefs().lock() {
            p.set(Self::KEY, &self);
        }
    }
}

/// A size given in pixels at the regular text size; it follows the text size setting like the theme's tokens.
pub const fn rem_px(px: f32) -> Rems {
    rems(px / 16.)
}

/// Every visual token in one place, so a light variant is another constructor.
pub struct Theme {
    pub window: Hsla,
    pub panel: Hsla,
    pub card: Hsla,
    pub card_hover: Hsla,
    pub hairline: Hsla,
    pub hairline_width: Pixels,
    pub tooltip: Hsla,
    pub selection: Hsla,
    /// The title bar's tab strip and its raised active segment.
    pub strip: Hsla,
    pub strip_border: Hsla,
    pub strip_active: Hsla,
    /// Bordered round controls such as the refresh pill.
    pub control: Hsla,
    pub control_border: Hsla,
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

    pub text_size: TextSize,
    /// Body text and the medium and semibold accents; one step heavier at the bold text size.
    pub weight_body: FontWeight,
    pub weight_medium: FontWeight,
    pub weight_semibold: FontWeight,
}

impl Global for Theme {}

fn white(alpha: f32) -> Hsla {
    gpui::white().opacity(alpha)
}

impl Theme {
    pub fn dark() -> Self {
        Self::dark_at(TextSize::Regular)
    }

    /// Sizes are written at the regular text size and scaled with the root rem size.
    pub fn dark_at(text_size: TextSize) -> Self {
        let scale = text_size.rem_size() / px(16.);
        let sz = |v: f32| px(v * scale);
        let weight = |w: FontWeight| FontWeight((w.0 + text_size.weight_step()).min(900.));
        Self {
            window: rgb(0x21232F).into(),
            panel: white(0.05),
            card: white(0.05),
            card_hover: white(0.08),
            hairline: white(0.08),
            hairline_width: px(0.5),
            tooltip: rgb(0x2D2F3B).into(),
            selection: white(0.13),
            strip: white(0.06),
            strip_border: white(0.10),
            strip_active: white(0.14),
            control: white(0.04),
            control_border: white(0.12),
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
            panel_title: sz(13.),
            card_label: sz(10.),
            card_value: sz(15.),
            caption: sz(11.),
            small_caption: sz(10.),
            axis: sz(9.),

            page_padding: sz(20.),
            gap: sz(16.),
            panel_padding: sz(16.),
            card_padding_y: sz(11.),
            card_padding_x: sz(13.),
            rail_width: sz(300.),

            panel_radius: sz(12.),
            card_radius: sz(10.),
            sub_radius: sz(8.),

            text_size,
            weight_body: weight(FontWeight::NORMAL),
            weight_medium: weight(FontWeight::MEDIUM),
            weight_semibold: weight(FontWeight::SEMIBOLD),
        }
    }
}

pub fn theme(cx: &App) -> &Theme {
    cx.global::<Theme>()
}

/// Sets the theme for `size`, and gpui-component's root rem size, which it applies to each window every frame.
pub fn install(size: TextSize, cx: &mut App) {
    gpui_component::Theme::global_mut(cx).font_size = size.rem_size();
    cx.set_global(Theme::dark_at(size));
}

/// Applies a text size to every open window in one re-layout, and remembers it.
pub fn set_text_size(size: TextSize, cx: &mut App) {
    size.save();
    install(size, cx);
    cx.refresh_windows();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::prefs::Prefs;

    #[test]
    fn each_text_size_scales_the_layout_with_the_text() {
        let regular = Theme::dark_at(TextSize::Regular);
        assert_eq!(regular.caption, px(11.), "regular is the design as drawn");
        let bigger = Theme::dark_at(TextSize::Bigger);
        assert!(
            (bigger.caption / regular.caption - 1.15).abs() < 0.01,
            "about 15% larger"
        );
        assert_eq!(
            bigger.caption / regular.caption,
            bigger.page_padding / regular.page_padding,
            "padding grows with the text so proportions hold"
        );
        assert_eq!(
            rem_px(11.).to_pixels(TextSize::Bigger.rem_size()),
            bigger.caption,
            "rem sizes and tokens agree"
        );
        assert_eq!(
            bigger.hairline_width, regular.hairline_width,
            "a hairline stays a hairline"
        );
    }

    #[test]
    fn bold_is_about_a_pixel_larger_and_a_weight_heavier() {
        let (regular, bold) = (Theme::dark_at(TextSize::Regular), Theme::dark_at(TextSize::Bold));
        for (r, b) in [(regular.panel_title, bold.panel_title), (regular.caption, bold.caption)] {
            let grown = b - r;
            assert!(grown >= px(0.75) && grown <= px(1.25), "{grown:?}");
        }
        assert_eq!(bold.weight_body, FontWeight::MEDIUM);
        assert_eq!(bold.weight_medium, FontWeight::SEMIBOLD);
        assert_eq!(regular.weight_body, FontWeight::NORMAL);
        assert_eq!(
            Theme::dark_at(TextSize::Bigger).weight_medium,
            FontWeight::MEDIUM,
            "bigger only grows"
        );
    }

    #[test]
    fn the_text_size_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        Prefs::load(path.clone()).set(TextSize::KEY, &TextSize::Bigger);
        assert_eq!(
            Prefs::load(path.clone()).get::<TextSize>(TextSize::KEY),
            Some(TextSize::Bigger)
        );
        std::fs::write(&path, "\"display.text_size\" = \"huge\"\n").unwrap();
        assert_eq!(
            Prefs::load(path).get::<TextSize>(TextSize::KEY),
            None,
            "an unknown level falls back to regular"
        );
    }
}
