use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use gpui::{
    App, FontFeatures, FontWeight, Global, Hsla, Pixels, Rems, SharedString, Styled, WindowAppearance, px, relative,
    rems, rgb,
};
use gpui_component::ThemeMode;
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

/// Light, dark, or whichever the desktop prefers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub const ALL: [Appearance; 3] = [Self::System, Self::Light, Self::Dark];
    const KEY: &str = "display.appearance";

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    /// The palette to draw with; dark, the app's own look, while the desktop has not said.
    pub fn resolve(self, desktop: Option<ThemeMode>) -> ThemeMode {
        match self {
            Self::Light => ThemeMode::Light,
            Self::Dark => ThemeMode::Dark,
            Self::System => desktop.unwrap_or(ThemeMode::Dark),
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

const RAIL_SHARE: f32 = 0.3;

/// A size given in pixels at the regular text size; it follows the text size setting like the theme's tokens.
pub const fn rem_px(px: f32) -> Rems {
    rems(px / 16.)
}

/// Every visual token in one place, in a dark and a light palette.
pub struct Theme {
    pub mode: ThemeMode,
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
    /// Lightens on dark and darkens on light; drawn at a low opacity for stripes, washes and outlines.
    pub wash: Hsla,
    /// Dims the window behind a dialog or sheet.
    pub scrim: Hsla,

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
    pub rail_max_width: Pixels,

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

/// The desktop's default sans and monospace families, as fontconfig resolves them.
fn system_fonts() -> &'static (SharedString, SharedString) {
    static FONTS: OnceLock<(SharedString, SharedString)> = OnceLock::new();
    FONTS.get_or_init(|| (fontconfig_family("sans-serif"), fontconfig_family("monospace")))
}

// gpui maps its own system-font name to a fixed family on Linux, so ask fontconfig for the user's choice
fn fontconfig_family(generic: &str) -> SharedString {
    Command::new("fc-match")
        .args(["--format=%{family[0]}", generic])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .filter(|family| !family.trim().is_empty())
        .map_or_else(|| ".SystemUIFont".into(), |family| family.trim().to_owned().into())
}

fn white(alpha: f32) -> Hsla {
    gpui::white().opacity(alpha)
}

fn black(alpha: f32) -> Hsla {
    gpui::black().opacity(alpha)
}

impl Theme {
    /// Sizes a screen's right rail with its gutters: a share of the row it ends, between `rail_width` and
    /// `rail_max_width` wide inside the gutters.
    pub fn rail<E: Styled>(&self, rail: E) -> E {
        let gutters = self.gap + self.page_padding;
        rail.w(relative(RAIL_SHARE))
            .min_w(self.rail_width + gutters)
            .max_w(self.rail_max_width + gutters)
    }

    pub fn dark() -> Self {
        Self::dark_at(TextSize::Regular)
    }

    pub fn at(mode: ThemeMode, text_size: TextSize) -> Self {
        match mode {
            ThemeMode::Dark => Self::dark_at(text_size),
            ThemeMode::Light => Self::light_at(text_size),
        }
    }

    /// Sizes are written at the regular text size and scaled with the root rem size.
    pub fn dark_at(text_size: TextSize) -> Self {
        let scale = text_size.rem_size() / px(16.);
        let sz = |v: f32| px(v * scale);
        let weight = |w: FontWeight| FontWeight((w.0 + text_size.weight_step()).min(900.));
        Self {
            mode: ThemeMode::Dark,
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
            wash: gpui::white(),
            scrim: black(0.5),

            green: rgb(0x30D158).into(),
            orange: rgb(0xFF9F0A).into(),
            red: rgb(0xFF453A).into(),
            blue: rgb(0x0A84FF).into(),
            teal: rgb(0x40C8E0).into(),
            purple: rgb(0xBF5AF2).into(),
            pink: rgb(0xFF375F).into(),
            indigo: rgb(0x5E5CE6).into(),

            font: system_fonts().0.clone(),
            mono: system_fonts().1.clone(),
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
            rail_max_width: sz(440.),

            panel_radius: sz(12.),
            card_radius: sz(10.),
            sub_radius: sz(8.),

            text_size,
            weight_body: weight(FontWeight::NORMAL),
            weight_medium: weight(FontWeight::MEDIUM),
            weight_semibold: weight(FontWeight::SEMIBOLD),
        }
    }

    /// White surfaces on a cool grey window; accents a shade deeper than on dark so they hold contrast on white.
    pub fn light_at(text_size: TextSize) -> Self {
        Self {
            mode: ThemeMode::Light,
            window: rgb(0xF0F1F5).into(),
            panel: rgb(0xFFFFFF).into(),
            card: black(0.035),
            card_hover: black(0.06),
            hairline: black(0.10),
            tooltip: rgb(0xFFFFFF).into(),
            selection: black(0.08),
            strip: black(0.05),
            strip_border: black(0.06),
            strip_active: rgb(0xFFFFFF).into(),
            control: rgb(0xFFFFFF).into(),
            control_border: black(0.12),
            text: black(0.88),
            text_secondary: black(0.58),
            text_tertiary: black(0.42),
            wash: gpui::black(),
            scrim: black(0.3),

            green: rgb(0x24A148).into(),
            orange: rgb(0xE07400).into(),
            red: rgb(0xE0352B).into(),
            blue: rgb(0x0A6CF0).into(),
            teal: rgb(0x1A9BB5).into(),
            purple: rgb(0xA44CD8).into(),
            pink: rgb(0xE5306A).into(),
            indigo: rgb(0x5552D9).into(),
            ..Self::dark_at(text_size)
        }
    }
}

pub fn theme(cx: &App) -> &Theme {
    cx.global::<Theme>()
}

/// The appearance setting and the desktop's preference, from which the palette is resolved.
#[derive(Default)]
struct Look {
    appearance: Appearance,
    desktop: Option<ThemeMode>,
    /// What the window last reported.
    window: Option<ThemeMode>,
}

impl Global for Look {}

/// Reads the desktop's preference and installs the saved appearance and text size, before any window opens.
pub fn init(cx: &mut App) {
    cx.set_global(Look {
        desktop: portal_preference(),
        ..Look::default()
    });
    install(Appearance::load(), TextSize::load(), cx);
}

/// Applies a palette and text size here and in gpui-component, and redraws every open window.
pub fn install(appearance: Appearance, size: TextSize, cx: &mut App) {
    let look = cx.default_global::<Look>();
    look.appearance = appearance;
    let mode = appearance.resolve(look.desktop);
    let theme = Theme::at(mode, size);
    gpui_component::Theme::change(mode, None, cx);
    let component = gpui_component::Theme::global_mut(cx);
    // gpui-component applies its root rem size to each window every frame
    component.font_size = size.rem_size();
    component.font_family = theme.font.clone();
    component.mono_font_family = theme.mono.clone();
    macro_rules! component_colour {
        ($($field:ident = $value:expr),*) => {$(
            component.colors.$field = $value;
            component.tokens.$field = $value.into();
        )*};
    }
    let clear = gpui::transparent_black();
    component_colour!(
        table = clear,
        table_head = clear,
        table_row_border = clear,
        table_even = theme.wash.opacity(0.03),
        table_hover = theme.wash.opacity(0.05),
        table_active = theme.selection,
        // the default dialog backdrop is too faint to read as dimmed
        overlay = theme.scrim
    );
    cx.set_global(theme);
    cx.refresh_windows();
}

pub fn appearance(cx: &App) -> Appearance {
    cx.try_global::<Look>().map(|l| l.appearance).unwrap_or_default()
}

/// Applies an appearance to every open window, and remembers it.
pub fn set_appearance(appearance: Appearance, cx: &mut App) {
    appearance.save();
    install(appearance, theme(cx).text_size, cx);
}

/// Applies a text size to every open window in one re-layout, and remembers it.
pub fn set_text_size(size: TextSize, cx: &mut App) {
    size.save();
    install(appearance(cx), size, cx);
}

/// Follows the desktop's light or dark switch, as the window reports it, while the appearance is System.
pub fn window_appearance(appearance: WindowAppearance, cx: &mut App) {
    let look = cx.default_global::<Look>();
    if !look.window_reported(appearance.into()) {
        return;
    }
    let setting = look.appearance;
    if setting.resolve(look.desktop) != theme(cx).mode {
        install(setting, theme(cx).text_size, cx);
    }
}

impl Look {
    /// Only a change counts: gpui repeats its own light default on unrelated window events until the portal answers.
    fn window_reported(&mut self, mode: ThemeMode) -> bool {
        let before = self.window.replace(mode);
        let changed = before.is_some_and(|before| before != mode);
        if changed {
            self.desktop = Some(mode);
        }
        changed
    }
}

// gpui hears the desktop's preference only after the first window is drawn, so ask up front to open in the right palette
fn portal_preference() -> Option<ThemeMode> {
    let bus = zbus::blocking::connection::Builder::session()
        .ok()?
        .method_timeout(Duration::from_millis(500))
        .build()
        .ok()?;
    let reply = bus
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Settings"),
            "ReadOne",
            &("org.freedesktop.appearance", "color-scheme"),
        )
        .ok()?;
    let value: zbus::zvariant::OwnedValue = reply.body().deserialize().ok()?;
    color_scheme(u32::try_from(value).ok()?)
}

/// The portal's `color-scheme`, read as gpui reads it: no preference (0) is light, as on GNOME's default style.
fn color_scheme(value: u32) -> Option<ThemeMode> {
    match value {
        1 => Some(ThemeMode::Dark),
        0 | 2 => Some(ThemeMode::Light),
        _ => None,
    }
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

    #[test]
    fn system_follows_the_desktop_and_is_dark_when_it_has_not_said() {
        use ThemeMode::{Dark, Light};
        assert_eq!(Appearance::default(), Appearance::System);
        assert_eq!(Appearance::System.resolve(Some(Light)), Light);
        assert_eq!(Appearance::System.resolve(Some(Dark)), Dark);
        assert_eq!(
            Appearance::System.resolve(None),
            Dark,
            "no portal answer keeps the app's own look"
        );
        for desktop in [None, Some(Light), Some(Dark)] {
            assert_eq!(
                Appearance::Light.resolve(desktop),
                Light,
                "a choice overrides the desktop"
            );
            assert_eq!(Appearance::Dark.resolve(desktop), Dark);
        }
    }

    #[test]
    fn only_a_change_in_the_window_appearance_is_the_desktop_speaking() {
        use ThemeMode::{Dark, Light};
        let mut look = Look::default();
        assert!(!look.window_reported(Light), "the first report is gpui's default");
        assert!(!look.window_reported(Light), "a repeat on an unrelated window event");
        assert_eq!(
            look.appearance.resolve(look.desktop),
            Dark,
            "with no portal, System stays dark"
        );
        assert!(look.window_reported(Dark));
        assert_eq!(look.desktop, Some(Dark));
        assert!(look.window_reported(Light), "the desktop's toggle");
        assert_eq!(look.appearance.resolve(look.desktop), Light);
    }

    #[test]
    fn the_portal_colour_scheme_reads_as_gpui_reads_it() {
        assert_eq!(color_scheme(1), Some(ThemeMode::Dark));
        assert_eq!(color_scheme(2), Some(ThemeMode::Light));
        assert_eq!(
            color_scheme(0),
            Some(ThemeMode::Light),
            "matches the live updates, which report no preference as light"
        );
        assert_eq!(color_scheme(7), None);
    }

    #[test]
    fn the_appearance_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        Prefs::load(path.clone()).set(Appearance::KEY, &Appearance::Light);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            "\"display.appearance\" = \"light\"",
            "readable when edited by hand"
        );
        assert_eq!(
            Prefs::load(path.clone()).get::<Appearance>(Appearance::KEY),
            Some(Appearance::Light)
        );
        std::fs::write(&path, "\"display.appearance\" = \"sepia\"\n").unwrap();
        assert_eq!(Prefs::load(path).get::<Appearance>(Appearance::KEY), None);
    }

    #[test]
    fn the_light_palette_replaces_every_colour_and_keeps_every_size() {
        let (dark, light) = (
            Theme::at(ThemeMode::Dark, TextSize::Bigger),
            Theme::at(ThemeMode::Light, TextSize::Bigger),
        );
        let colours = |t: &Theme| {
            [
                t.window,
                t.panel,
                t.card,
                t.card_hover,
                t.hairline,
                t.tooltip,
                t.selection,
                t.strip,
                t.strip_border,
                t.strip_active,
                t.control,
                t.control_border,
                t.text,
                t.text_secondary,
                t.text_tertiary,
                t.wash,
                t.scrim,
                t.green,
                t.orange,
                t.red,
                t.blue,
                t.teal,
                t.purple,
                t.pink,
                t.indigo,
            ]
        };
        // a colour left out of the light constructor would silently keep its dark value
        for (i, (d, l)) in colours(&dark).into_iter().zip(colours(&light)).enumerate() {
            assert_ne!(d, l, "colour {i} is the same in both palettes");
        }
        assert!(light.window.l > 0.9 && dark.window.l < 0.2);
        assert!(
            light.text.l < 0.1 && dark.text.l > 0.9,
            "text contrasts with its window"
        );
        assert_eq!((dark.mode, light.mode), (ThemeMode::Dark, ThemeMode::Light));
        assert_eq!(light.caption, dark.caption);
        assert_eq!(light.page_padding, dark.page_padding);
        assert_eq!(light.weight_body, dark.weight_body);
    }

    #[test]
    fn light_accents_hold_contrast_on_white() {
        fn luminance(c: Hsla) -> f32 {
            let c = c.to_rgb();
            let lin = |v: f32| {
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
        }
        let t = Theme::light_at(TextSize::Regular);
        // status words such as "Running" are drawn in the accents, so they must read as text, not only as marks
        for (name, c) in [
            ("green", t.green),
            ("orange", t.orange),
            ("red", t.red),
            ("blue", t.blue),
            ("teal", t.teal),
            ("purple", t.purple),
            ("pink", t.pink),
            ("indigo", t.indigo),
        ] {
            let ratio = 1.05 / (luminance(c) + 0.05);
            assert!(ratio >= 3., "{name} is {ratio:.2}:1 on white");
        }
    }
}
