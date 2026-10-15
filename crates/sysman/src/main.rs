use gpui::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions, px, size};
use gpui_component::{Root, ThemeMode};

use sysman::ui;
use sysman::ui::shell::Shell;
use sysman::ui::theme::Theme;

fn main() {
    gpui_platform::application()
        .with_assets(ui::Assets)
        .run(|cx: &mut App| {
            gpui_component::init(cx);
            gpui_component::Theme::change(ThemeMode::Dark, None, cx);
            let theme = Theme::dark();
            let component = gpui_component::Theme::global_mut(cx);
            component.font_family = theme.font.clone();
            component.mono_font_family = theme.mono.clone();
            cx.set_global(theme);

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.), px(720.)),
                    cx,
                ))),
                window_min_size: Some(size(px(860.), px(520.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("sysman".into()),
                    appears_transparent: true,
                    traffic_light_position: None,
                }),
                app_id: Some("sysman".into()),
                window_decorations: Some(WindowDecorations::Client),
                ..Default::default()
            };
            cx.open_window(options, |window, cx| {
                let shell = cx.new(|cx| Shell::new(window, cx));
                cx.new(|cx| Root::new(shell, window, cx))
            })
            .expect("open window");
            cx.activate(true);
        });
}
