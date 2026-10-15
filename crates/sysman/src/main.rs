use gpui::{
    App, AppContext, Bounds, Entity, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions, px, size,
};
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
            macro_rules! table_colour {
                ($($field:ident = $value:expr),*) => {$(
                    component.colors.$field = $value;
                    component.tokens.$field = $value.into();
                )*};
            }
            let clear = gpui::transparent_black();
            table_colour!(
                table = clear,
                table_head = clear,
                table_row_border = clear,
                table_even = gpui::white().opacity(0.03),
                table_hover = gpui::white().opacity(0.05),
                table_active = theme.selection
            );
            cx.set_global(theme);

            let shell = cx.new(Shell::new);
            #[cfg(debug_assertions)]
            if std::env::var_os("SYSMAN_DEV_OPEN_AFTER_DATA").is_some() {
                open_when_ready(shell, cx);
                return;
            }
            open(shell, cx);
        });
}

/// Development aid: a locked Hyprland session draws only a window's first frame, so open once that frame has data.
#[cfg(debug_assertions)]
fn open_when_ready(shell: Entity<Shell>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let started = std::time::Instant::now();
        while !cx.update(|cx| shell.read(cx).ready(cx)) {
            if started.elapsed() > std::time::Duration::from_secs(20) {
                eprintln!("SYSMAN_DEV_OPEN_AFTER_DATA: not ready after 20 s (no daemon, or SYSMAN_DEV_SELECT matches nothing); opening anyway");
                break;
            }
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
        }
        cx.update(|cx| open(shell, cx));
    })
    .detach();
}

fn open(shell: Entity<Shell>, cx: &mut App) {
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
        app_id: Some(app_id().into()),
        window_decorations: Some(WindowDecorations::Client),
        ..Default::default()
    };
    cx.open_window(options, |window, cx| {
        window.set_window_title("sysman");
        cx.new(|cx| Root::new(shell, window, cx))
    })
    .expect("open window");
    cx.activate(true);
}

/// Debug builds can take another app id so a compositor rule can route test windows off-screen.
fn app_id() -> String {
    #[cfg(debug_assertions)]
    if let Ok(id) = std::env::var("SYSMAN_DEV_APP_ID") {
        return id;
    }
    "sysman".to_string()
}
