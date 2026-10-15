use std::rc::Rc;

use gpui::{
    App, Bounds, IntoElement, ParentElement, PathBuilder, Pixels, RenderOnce, SharedString, Styled, Window, canvas,
    div, point, prelude::FluentBuilder, px,
};

use crate::charts::trend::{dot, monotone, runs};
use crate::data::history::Bucket;
use crate::ui::theme::theme;

pub const HEIGHT: Pixels = px(57.);
/// Room above the line for the caption.
const TOP: Pixels = px(15.);

#[derive(IntoElement)]
pub struct Sparkline {
    data: Rc<[Bucket]>,
    color: gpui::Hsla,
    span: (f64, f64),
    width_s: f64,
    gap_s: f64,
    fixed: Option<(f64, SharedString)>,
}

impl Sparkline {
    pub fn new(data: Rc<[Bucket]>, color: gpui::Hsla, span: (f64, f64), width_s: f64, gap_s: f64) -> Self {
        Self {
            data,
            color,
            span,
            width_s,
            gap_s,
            fixed: None,
        }
    }

    /// Scale from zero to `max` instead of to the data, and say so.
    pub fn fixed(mut self, max: f64, label: impl Into<SharedString>) -> Self {
        self.fixed = Some((max, label.into()));
        self
    }

    fn paint(&self, bounds: Bounds<Pixels>, window: &mut Window) {
        let (from, to) = self.span;
        let visible: Vec<&Bucket> = self
            .data
            .iter()
            .filter(|b| b.t as f64 + self.width_s >= from && b.t as f64 <= to)
            .collect();
        let max = match &self.fixed {
            Some((max, _)) => *max,
            None => visible.iter().map(|b| b.avg).fold(0., f64::max),
        }
        .max(f64::EPSILON);
        let (left, width) = (f32::from(bounds.left()), f32::from(bounds.size.width) - 4.);
        let (bottom, height) = (
            f32::from(bounds.bottom()) - 3.,
            f32::from(bounds.size.height - TOP) - 4.,
        );
        let at = |b: &Bucket| {
            let t = (b.t as f64 + self.width_s / 2.).min(to);
            let x = left + width * ((t - from) / (to - from).max(1.)) as f32;
            (x, bottom - height * (b.avg / max).clamp(0., 1.) as f32)
        };
        let ts: Vec<i64> = visible.iter().map(|b| b.t).collect();
        let mut end = None;
        for run in runs(&ts, self.gap_s) {
            let p: Vec<(f32, f32)> = run.map(|i| at(visible[i])).collect();
            end = p.last().copied();
            let mut path = PathBuilder::stroke(px(1.5));
            path.move_to(point(px(p[0].0), px(p[0].1)));
            for [c1, c2, e] in monotone(&p) {
                path.cubic_bezier_to(
                    point(px(e.0), px(e.1)),
                    point(px(c1.0), px(c1.1)),
                    point(px(c2.0), px(c2.1)),
                );
            }
            if let Ok(path) = path.build() {
                window.paint_path(path, self.color);
            }
        }
        if let Some((x, y)) = end {
            window.paint_quad(dot(point(px(x), px(y)), px(2.5), self.color));
        }
    }
}

impl RenderOnce for Sparkline {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = theme(cx);
        let caption = self.fixed.as_ref().map(|(_, label)| format!("Axis max {label}"));
        let (size, color, numbers) = (t.small_caption, t.text_tertiary, t.numbers.clone());
        div()
            .relative()
            .w_full()
            .h(HEIGHT)
            .when_some(caption, |d, caption| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .text_size(size)
                        .text_color(color)
                        .font_features(numbers)
                        .child(caption),
                )
            })
            .child(canvas(|_, _, _| (), move |bounds, (), window, _| self.paint(bounds, window)).size_full())
    }
}
