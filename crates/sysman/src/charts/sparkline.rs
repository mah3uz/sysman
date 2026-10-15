use std::rc::Rc;

use gpui::{
    App, Bounds, ElementId, Entity, IntoElement, ParentElement, Path, PathBuilder, Pixels, Point, RenderOnce,
    SharedString, Styled, Window, canvas, div, point, prelude::FluentBuilder, px,
};

use crate::charts::trend::{dot, monotone, runs};
use crate::data::history::Bucket;
use crate::ui::theme::theme;

pub const HEIGHT: Pixels = px(57.);
/// Room above the line for the caption.
const TOP: Pixels = px(15.);

/// Everything the built strokes depend on besides the data, which is compared by identity.
type Key = ((f64, f64), f64, f64, Option<f64>, bool, Bounds<Pixels>);

/// Strokes and end dot kept between frames while the data and size stay the same.
struct Built {
    data: Rc<[Bucket]>,
    key: Key,
    paths: Vec<Path<Pixels>>,
    end: Option<Point<Pixels>>,
}

#[derive(IntoElement)]
pub struct Sparkline {
    id: ElementId,
    data: Rc<[Bucket]>,
    color: gpui::Hsla,
    span: (f64, f64),
    width_s: f64,
    gap_s: f64,
    fixed: Option<(f64, SharedString)>,
    caption: Option<SharedString>,
    fit: bool,
}

impl Sparkline {
    /// `id` keeps the built strokes across frames, so it must be unique among the sparklines drawn together.
    pub fn new(
        id: impl Into<ElementId>,
        data: Rc<[Bucket]>,
        color: gpui::Hsla,
        span: (f64, f64),
        width_s: f64,
        gap_s: f64,
    ) -> Self {
        Self {
            id: id.into(),
            data,
            color,
            span,
            width_s,
            gap_s,
            fixed: None,
            caption: None,
            fit: false,
        }
    }

    /// Scale from zero to `max` instead of to the data, and say so.
    pub fn fixed(mut self, max: f64, label: impl Into<SharedString>) -> Self {
        self.fixed = Some((max, label.into()));
        self
    }

    /// Scale from the data's lowest to its highest value, so a slow climb fills the strip.
    pub fn fit(mut self) -> Self {
        self.fit = true;
        self
    }

    /// Top-right caption in place of the fixed axis note, such as "peak 97%".
    pub fn caption(mut self, caption: impl Into<SharedString>) -> Self {
        self.caption = Some(caption.into());
        self
    }

    fn key(&self, bounds: Bounds<Pixels>) -> Key {
        let max = self.fixed.as_ref().map(|(max, _)| *max);
        (self.span, self.width_s, self.gap_s, max, self.fit, bounds)
    }

    fn paint(&self, bounds: Bounds<Pixels>, cache: &Entity<Option<Built>>, window: &mut Window, cx: &mut App) {
        let key = self.key(bounds);
        cache.update(cx, |built, _| {
            if built
                .as_ref()
                .is_none_or(|b| b.key != key || !Rc::ptr_eq(&b.data, &self.data))
            {
                *built = Some(self.build(bounds));
            }
            let built = built.as_ref().expect("built above");
            for path in &built.paths {
                window.paint_path(path.clone(), self.color);
            }
            if let Some(end) = built.end {
                window.paint_quad(dot(end, px(2.5), self.color));
            }
        });
    }

    fn build(&self, bounds: Bounds<Pixels>) -> Built {
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
        let min = if self.fit {
            visible.iter().map(|b| b.avg).fold(f64::INFINITY, f64::min).min(max)
        } else {
            0.
        };
        let range = (max - min).max(f64::EPSILON);
        let (left, width) = (f32::from(bounds.left()), f32::from(bounds.size.width) - 4.);
        let (bottom, height) = (
            f32::from(bounds.bottom()) - 3.,
            f32::from(bounds.size.height - TOP) - 4.,
        );
        let at = |b: &Bucket| {
            let t = (b.t as f64 + self.width_s / 2.).min(to);
            let x = left + width * ((t - from) / (to - from).max(1.)) as f32;
            (x, bottom - height * ((b.avg - min) / range).clamp(0., 1.) as f32)
        };
        let ts: Vec<i64> = visible.iter().map(|b| b.t).collect();
        let mut built = Built {
            data: self.data.clone(),
            key: self.key(bounds),
            paths: Vec::new(),
            end: None,
        };
        for run in runs(&ts, self.gap_s) {
            let p: Vec<(f32, f32)> = run.map(|i| at(visible[i])).collect();
            built.end = p.last().map(|&(x, y)| point(px(x), px(y)));
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
                built.paths.push(path);
            }
        }
        built
    }
}

impl RenderOnce for Sparkline {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let cache = window.use_keyed_state(self.id.clone(), cx, |_, _| None);
        let t = theme(cx);
        let caption = self
            .caption
            .clone()
            .or_else(|| self.fixed.as_ref().map(|(_, label)| format!("Axis max {label}").into()));
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
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, cx| self.paint(bounds, &cache, window, cx),
                )
                .size_full(),
            )
    }
}
