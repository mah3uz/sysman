use gpui::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement, Pixels,
    Point, StatefulInteractiveElement, Styled, canvas, div, prelude::FluentBuilder, px, relative,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{Disableable, Icon, Sizable};
use gpui_kit_assets::IconName;

use super::model::{self, Span, thousands};
use super::toolbar::hair_b;
use super::{Item, Logs};
use crate::data::logs::now_us;
use crate::ui::theme::{rem_px, theme};
use crate::ui::widgets::range::Range;
use crate::ui::widgets::segmented::segmented;

const BARS_HEIGHT: f32 = 40.;
/// A drag narrower than this share of the strip is a click.
const MIN_SLICE: f32 = 0.004;

impl Logs {
    /// How the scope reads mid-sentence: "this boot", "the last 24 hr", "boot −1".
    pub(super) fn scope_name(&self) -> String {
        match &self.span {
            Span::Last(r) => format!("the last {}", r.label()),
            Span::ThisBoot => "this boot".into(),
            Span::Boot(id) => self.boot_name(id).to_lowercase(),
        }
    }

    pub(super) fn boot_name(&self, id: &str) -> String {
        let boots = &self.view.boots;
        boots
            .iter()
            .position(|b| b.id == id)
            .map_or_else(|| "Earlier boot".into(), |i| model::boot_label(i, boots))
    }

    pub(super) fn header(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let mut options: Vec<(Span, String)> = Range::ALL
            .into_iter()
            .map(|r| (Span::Last(r), r.label().to_string()))
            .collect();
        options.push((Span::ThisBoot, "This boot".into()));
        if let Span::Boot(id) = &self.span {
            options.push((self.span.clone(), self.boot_name(id)));
        }
        let this = cx.entity().downgrade();
        let state = self.scan.state();
        let progress = state.is_running().then(|| {
            let (from, now) = (self.view.range.0, now_us());
            let at = self.scan.position_us();
            if from == 0 || at < from || now <= from {
                0.
            } else {
                ((at - from) as f32 / (now - from) as f32).clamp(0., 1.)
            }
        });
        div()
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .gap(rem_px(12.))
            .h(rem_px(38.))
            .px(rem_px(14.))
            .child(segmented(
                "logs-range",
                options,
                &self.span,
                move |span, _, cx| {
                    this.update(cx, |this, cx| this.set_span(span, cx)).ok();
                },
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(t.caption)
                    .text_color(t.text_secondary)
                    .font_features(t.numbers.clone())
                    .child(self.summary()),
            )
            .children(progress.map(|f| {
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(px(2.))
                    .bg(t.blue.opacity(0.12))
                    .child(div().h_full().w(relative(f)).bg(t.blue))
            }))
    }

    fn summary(&self) -> String {
        let state = self.scan.state();
        let mut out = if state.is_running() {
            let at = self.scan.position_us();
            let mut s = format!("Reading the journal · {} entries", thousands(self.scan.entries_read()));
            if at > 0 {
                s.push_str(&format!(" · at {}", model::clock(at, "%b %-d %H:%M:%S", &self.tz)));
            }
            s
        } else {
            let total = self.view.key.rows as u64;
            let shown = self.view.entries;
            let mut s = if self.query.is_empty() {
                format!("{} entries in {}", thousands(shown), self.scope_name())
            } else {
                format!("{} of {} entries in {}", thousands(shown), thousands(total), self.scope_name())
            };
            let (from, to) = self.view.range;
            if to > from {
                let pattern = if model::same_day(from, to, &self.tz) { "%H:%M:%S" } else { "%b %-d %H:%M" };
                s.push_str(&format!(
                    " · {} – {}",
                    model::clock(from, pattern, &self.tz),
                    model::clock(to, pattern, &self.tz)
                ));
            }
            let rows = self.view.items.iter().filter(|i| matches!(i, Item::Entry(_))).count() as u64;
            if rows < shown {
                s.push_str(&format!(" · {} rows after folding", thousands(rows)));
            }
            s
        };
        if let Some(search) = self.search.as_ref().filter(|s| s.state().is_running()) {
            let of = self.view.key.rows.max(1) as f64;
            out.push_str(&format!(
                " · searching messages {:.0}%",
                (search.rows_done() as f64 / of * 100.).min(100.)
            ));
        }
        out
    }

    fn strip_fraction(&self, at: Point<Pixels>) -> f32 {
        let b = self.strip.get();
        if b.size.width <= px(0.) {
            return 0.;
        }
        (f32::from(at.x - b.origin.x) / f32::from(b.size.width)).clamp(0., 1.)
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        let Some((a, b)) = self.drag.take() else { return };
        let (a, b) = (a.min(b), a.max(b));
        let (from, to) = self.view.range;
        if b - a >= MIN_SLICE && to > from {
            let at = |f: f32| from + ((to - from) as f64 * f as f64) as u64;
            self.set_controls(|c| c.window = Some((at(a), at(b))), cx);
        }
        cx.notify();
    }

    /// Entries per bucket over the scope, stacked by priority, with the dragged slice outlined.
    pub(super) fn strip(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let buckets = &self.view.buckets;
        let max = buckets.iter().map(|b| b.total()).max().unwrap_or(0).max(1) as f32;
        let colours = [t.text_tertiary.opacity(0.35), t.blue, t.orange, t.red];
        let (from, to) = self.view.range;
        let slice = match (self.drag, self.controls.window) {
            (Some((a, b)), _) => Some((a.min(b), a.max(b))),
            (None, Some((a, b))) if to > from => {
                let f = |x: u64| (x.saturating_sub(from) as f32 / (to - from) as f32).clamp(0., 1.);
                Some((f(a), f(b)))
            }
            _ => None,
        };
        let ticks = model::ticks(from, to, &self.tz).into_iter().map(|(f, label)| {
            div()
                .absolute()
                .left(relative(f))
                .top_0()
                .ml(rem_px(-20.))
                .w(rem_px(40.))
                .text_center()
                .text_size(t.axis)
                .text_color(t.text_tertiary)
                .font_features(t.numbers.clone())
                .child(label)
        });
        let cell = self.strip.clone();
        let bars = div()
            .id("logs-strip")
            .relative()
            .flex_1()
            .min_w_0()
            .h(rem_px(BARS_HEIGHT))
            .flex()
            .flex_row()
            .items_end()
            .gap(px(1.))
            .cursor_col_resize()
            .child(
                canvas(move |bounds, _, _| cell.set(bounds), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .children(buckets.iter().map(|b| {
                let total = b.total() as f32;
                let bands = [b.info, b.notices, b.warnings, b.errors];
                div()
                    .flex_1()
                    .h(relative(total / max))
                    .when(total > 0., |d| d.min_h(px(1.)))
                    .flex()
                    .flex_col_reverse()
                    .rounded_t(px(1.5))
                    .overflow_hidden()
                    .children(
                        bands
                            .into_iter()
                            .zip(colours)
                            .filter(|(v, _)| *v > 0)
                            .map(|(v, c)| div().w_full().h(relative(v as f32 / total)).bg(c)),
                    )
            }))
            .when(buckets.iter().all(|b| b.total() == 0), |d| {
                d.child(div().absolute().left_0().right_0().bottom_0().h(px(1.)).bg(t.hairline))
            })
            .children(slice.map(|(a, b)| {
                div()
                    .absolute()
                    .top(px(-2.))
                    .bottom(px(-2.))
                    .left(relative(a))
                    .w(relative((b - a).max(0.002)))
                    .rounded(rem_px(3.))
                    .border_1()
                    .border_color(t.blue.opacity(0.8))
                    .bg(t.blue.opacity(0.08))
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    if e.click_count >= 2 {
                        this.drag = None;
                        this.set_controls(|c| c.window = None, cx);
                        return;
                    }
                    let f = this.strip_fraction(e.position);
                    this.drag = Some((f, f));
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                if this.drag.is_none() {
                    return;
                }
                if e.pressed_button != Some(MouseButton::Left) {
                    this.end_drag(cx);
                    return;
                }
                let f = this.strip_fraction(e.position);
                if let Some(d) = this.drag.as_mut() {
                    d.1 = f;
                }
                cx.notify();
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| this.end_drag(cx)))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, cx| this.end_drag(cx)))
            .tooltip(|window, cx| {
                gpui_component::tooltip::Tooltip::new("Drag across to narrow the time · double-click to clear")
                    .build(window, cx)
            });
        hair_b(
            div()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap(rem_px(3.))
                .px(rem_px(14.))
                .pt(rem_px(4.))
                .pb(rem_px(6.)),
            t,
        )
        .child(
            div().flex().flex_row().items_end().gap(rem_px(8.)).child(bars).child(
                Button::new("logs-lanes")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::ChevronDown))
                    .disabled(true)
                    .tooltip("Lanes by category, with what sysman recorded: not available yet"),
            ),
        )
        .child(div().relative().h(rem_px(11.)).mr(rem_px(30.)).children(ticks))
    }
}
