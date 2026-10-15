use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, AppContext, Context, CursorStyle, Entity, EntityId, EventEmitter, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Styled, Window, canvas, div,
    prelude::FluentBuilder, px,
};
use gpui_component::tooltip::Tooltip;
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;
use serde::{Deserialize, Serialize};

use crate::data::prefs::{Prefs, prefs};
use crate::ui::theme::{Theme, theme};

const ANIMATION: Duration = Duration::from_millis(150);
/// What a collapsed sidebar leaves behind: a strip that brings it back.
const STRIP: f32 = 14.;
const SASH: f32 = 6.;
const GUTTER: f32 = 22.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Widths {
    pub default: f32,
    pub min: f32,
    pub max: f32,
}

impl Widths {
    pub const fn new(default: f32, min: f32, max: f32) -> Self {
        Self { default, min, max }
    }

    fn clamp(self, width: f32) -> f32 {
        if width.is_finite() {
            width.clamp(self.min, self.max)
        } else {
            self.default
        }
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Stored {
    width: f32,
    collapsed: bool,
}

/// Emitted when a sidebar collapses or expands, not while it is dragged.
pub struct Toggled;

/// A side panel's width and collapsed state, persisted under its key.
pub struct Sidebar {
    key: &'static str,
    side: Side,
    widths: Widths,
    width: f32,
    collapsed: bool,
    /// The width shown when the running collapse or expand began.
    from: Option<(f32, Instant)>,
    dragging: bool,
}

impl EventEmitter<Toggled> for Sidebar {}

impl Sidebar {
    /// `key` names it in the preferences, as `<screen>.<side>`.
    pub fn new(key: &'static str, side: Side, widths: Widths) -> Self {
        #[allow(unused_mut)]
        let mut this = Self::load(key, side, widths, &prefs().lock().unwrap_or_else(|e| e.into_inner()));
        #[cfg(debug_assertions)]
        if std::env::var("SYSMAN_DEV_COLLAPSE").is_ok_and(|keys| keys.split(',').any(|k| k == key)) {
            this.collapsed = true;
        }
        this
    }

    fn load(key: &'static str, side: Side, widths: Widths, prefs: &Prefs) -> Self {
        let stored = prefs.get::<Stored>(key);
        Self {
            key,
            side,
            widths,
            width: stored.as_ref().map_or(widths.default, |s| widths.clamp(s.width)),
            collapsed: stored.is_some_and(|s| s.collapsed),
            from: None,
            dragging: false,
        }
    }

    fn save(&self, prefs: &mut Prefs) {
        prefs.set(
            self.key,
            &Stored {
                width: self.width,
                collapsed: self.collapsed,
            },
        );
    }

    fn persist(&self) {
        self.save(&mut prefs().lock().unwrap_or_else(|e| e.into_inner()));
    }

    pub fn collapsed(&self) -> bool {
        self.collapsed
    }

    pub fn set_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if collapsed == self.collapsed {
            return;
        }
        self.from = Some((self.shown(Instant::now()), Instant::now()));
        self.collapsed = collapsed;
        self.persist();
        cx.emit(Toggled);
        cx.notify();
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        self.set_collapsed(!self.collapsed, cx);
    }

    /// The room it takes in the layout now, mid-animation included.
    pub fn width(&self) -> Pixels {
        px(self.shown(Instant::now()))
    }

    fn target(&self) -> f32 {
        if self.collapsed { STRIP } else { self.width }
    }

    fn shown(&self, now: Instant) -> f32 {
        let target = self.target();
        match self.from {
            Some((from, start)) => {
                let t = (now.saturating_duration_since(start).as_secs_f32() / ANIMATION.as_secs_f32()).min(1.);
                from + (target - from) * ease_out(t)
            }
            None => target,
        }
    }

    fn animating(&self, now: Instant) -> bool {
        self.from
            .is_some_and(|(_, start)| now.saturating_duration_since(start) < ANIMATION)
    }

    fn resize(&mut self, width: f32, cx: &mut Context<Self>) {
        let width = self.widths.clamp(width);
        if width != self.width {
            self.width = width;
            self.from = None;
            cx.notify();
        }
    }
}

fn ease_out(t: f32) -> f32 {
    1. - (1. - t).powi(3)
}

/// The value a resize drag carries; the sidebar it belongs to.
struct Resize(EntityId);

struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// The sidebar at its current width. `content` fills it and is laid out at the full width,
/// so a collapse or expand only clips it; it may be `None` while [`showing`] is false.
pub fn sidebar(state: &Entity<Sidebar>, content: Option<AnyElement>, window: &mut Window, cx: &App) -> AnyElement {
    let t = theme(cx);
    let s = state.read(cx);
    let now = Instant::now();
    let animating = s.animating(now);
    if animating {
        window.request_animation_frame();
    }
    let (key, side) = (s.key, s.side);
    if s.collapsed && !animating {
        return strip(state, key, side, t);
    }
    let id = state.entity_id();
    let width = s.width;
    let resizing = state.downgrade();
    let frame = div()
        .id(key)
        .relative()
        .flex_shrink_0()
        .h_full()
        .w(px(s.shown(now)))
        .on_drag_move::<Resize>(move |e, _, cx| {
            if e.drag(cx).0 != id {
                return;
            }
            let x = e.event.position.x;
            let w = match side {
                Side::Left => x - e.bounds.left(),
                Side::Right => e.bounds.right() - x,
            };
            resizing.update(cx, |s, cx| s.resize(f32::from(w), cx)).ok();
        })
        .child(
            div()
                .size_full()
                .overflow_hidden()
                .flex()
                .flex_row()
                .when(side == Side::Right, |d| d.justify_end())
                .child(div().flex_shrink_0().w(px(width)).h_full().children(content)),
        )
        // the sash's own cursor is lost as soon as the pointer outruns it
        .when(s.dragging, |d| {
            d.child(
                canvas(
                    |_, _, _| {},
                    |_, _, window, _| window.set_window_cursor_style(CursorStyle::ResizeLeftRight),
                )
                .absolute(),
            )
        });
    if s.collapsed {
        return frame.into_any_element();
    }
    frame.child(gutter(state, key, side, t)).into_any_element()
}

/// Whether the content is on screen, so a caller can skip building it while collapsed.
pub fn showing(state: &Entity<Sidebar>, cx: &App) -> bool {
    let s = state.read(cx);
    !s.collapsed || s.animating(Instant::now())
}

/// The inner edge: a resize handle, and a hide button that shows on hover.
fn gutter(state: &Entity<Sidebar>, key: &'static str, side: Side, t: &Theme) -> impl IntoElement + use<> {
    let (start, release, release_out, toggle) = (
        state.downgrade(),
        state.downgrade(),
        state.downgrade(),
        state.downgrade(),
    );
    let id = state.entity_id();
    let end_drag = |s: &mut Sidebar, cx: &mut Context<Sidebar>| {
        if std::mem::take(&mut s.dragging) {
            s.persist();
            cx.notify();
        }
    };
    let sash = div()
        .id("sidebar-sash")
        .absolute()
        .top_0()
        .bottom_0()
        .w(px(SASH))
        .map(|d| match side {
            Side::Left => d.right_0(),
            Side::Right => d.left_0(),
        })
        .cursor_col_resize()
        .hover(|d| d.bg(t.hairline))
        .on_drag(Resize(id), move |_, _, _, cx| {
            start
                .update(cx, |s, cx| {
                    s.dragging = true;
                    cx.notify();
                })
                .ok();
            cx.new(|_| DragGhost)
        })
        .on_mouse_up(MouseButton::Left, move |_, _, cx| {
            release.update(cx, end_drag).ok();
        })
        .on_mouse_up_out(MouseButton::Left, move |_, _, cx| {
            release_out.update(cx, end_drag).ok();
        });
    let toggle = div()
        .id("sidebar-hide")
        .w(px(12.))
        .h(px(36.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .bg(t.control)
        .border_1()
        .border_color(t.control_border)
        .text_color(t.text_secondary)
        .opacity(0.)
        .group_hover(SharedString::from(key), |d| d.opacity(1.))
        .hover(|d| d.text_color(t.text))
        .cursor_pointer()
        .child(
            Icon::new(match side {
                Side::Left => IconName::ChevronLeft,
                Side::Right => IconName::ChevronRight,
            })
            .xsmall(),
        )
        .tooltip(|window, cx| Tooltip::new("Hide sidebar").build(window, cx))
        .on_click(move |_, _, cx| {
            toggle.update(cx, |s, cx| s.set_collapsed(true, cx)).ok();
        });
    // Half the sash overhangs the edge, so it is as easy to grab from either side.
    let overhang = px(-SASH / 2.);
    div()
        .group(key)
        .absolute()
        .top_0()
        .bottom_0()
        .w(px(GUTTER))
        .map(|d| match side {
            Side::Left => d.right(overhang).items_start().pl(px(2.)),
            Side::Right => d.left(overhang).items_end().pr(px(2.)),
        })
        .flex()
        .flex_col()
        .justify_center()
        .child(toggle)
        .child(sash)
}

fn strip(state: &Entity<Sidebar>, key: &'static str, side: Side, t: &Theme) -> AnyElement {
    let expand = state.downgrade();
    let mut d = div()
        .id(key)
        .flex_shrink_0()
        .h_full()
        .w(px(STRIP))
        .flex()
        .flex_col()
        .items_center()
        .pt(px(12.))
        .bg(t.panel.opacity(0.5))
        .text_color(t.text_tertiary)
        .hover(|d| d.bg(t.card_hover).text_color(t.text))
        .cursor_pointer()
        .border_color(t.hairline)
        .child(
            Icon::new(match side {
                Side::Left => IconName::ChevronRight,
                Side::Right => IconName::ChevronLeft,
            })
            .xsmall(),
        )
        .tooltip(|window, cx| Tooltip::new("Show sidebar").build(window, cx))
        .on_click(move |_, _, cx| {
            expand.update(cx, |s, cx| s.set_collapsed(false, cx)).ok();
        });
    let edges = &mut d.style().border_widths;
    let hairline = Some(t.hairline_width.into());
    match side {
        Side::Left => edges.right = hairline,
        Side::Right => edges.left = hairline,
    }
    d.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Widths = Widths::new(280., 220., 420.);

    #[test]
    fn a_width_stays_between_its_bounds() {
        // a drag past either end, or a hand-edited file, must not hide the panel or swallow the screen
        assert_eq!(W.clamp(100.), 220.);
        assert_eq!(W.clamp(9000.), 420.);
        assert_eq!(W.clamp(300.), 300.);
        assert_eq!(W.clamp(f32::NAN), 280.);
        assert_eq!(W.clamp(f32::INFINITY), 280.);
    }

    #[test]
    fn width_and_collapsed_state_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        let mut prefs = Prefs::load(path.clone());
        let mut left = Sidebar::load("explorer.left", Side::Left, W, &prefs);
        assert_eq!(
            (left.width, left.collapsed),
            (280., false),
            "an unknown key starts at the default"
        );
        left.width = 350.;
        left.collapsed = true;
        left.save(&mut prefs);

        let prefs = Prefs::load(path);
        let left = Sidebar::load("explorer.left", Side::Left, W, &prefs);
        assert_eq!((left.width, left.collapsed), (350., true));
        let right = Sidebar::load("explorer.right", Side::Right, W, &prefs);
        assert_eq!(
            (right.width, right.collapsed),
            (280., false),
            "each side keeps its own state"
        );
    }

    #[test]
    fn a_stored_width_outside_new_bounds_is_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        std::fs::write(&path, "[\"groups.left\"]\nwidth = 900.0\ncollapsed = false\n").unwrap();
        let s = Sidebar::load("groups.left", Side::Left, W, &Prefs::load(path));
        assert_eq!(s.width, 420.);
    }

    #[test]
    fn a_collapse_eases_to_the_strip_and_then_stops_asking_for_frames() {
        let mut s = Sidebar::load("t.left", Side::Left, W, &Prefs::load(Default::default()));
        let start = Instant::now();
        s.from = Some((280., start));
        s.collapsed = true;
        assert_eq!(s.shown(start), 280.);
        let early = s.shown(start + ANIMATION / 4);
        let late = s.shown(start + ANIMATION * 3 / 4);
        assert!(early < 280. && late < early && late > STRIP);
        // ease-out: most of the distance is covered in the first half
        assert!(280. - early > (280. - STRIP) / 4.);
        assert!(s.animating(start + ANIMATION / 2));
        assert!(!s.animating(start + ANIMATION));
        assert_eq!(s.shown(start + ANIMATION * 2), STRIP);
    }
}
