use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, Background, BorderStyle, Bounds, ContentMask, DispatchPhase, Font, Hitbox, HitboxBehavior, Hsla, IntoElement,
    MouseMoveEvent, ParentElement, Path, PathBuilder, Pixels, Point, RenderOnce, SharedString, Styled, TextAlign,
    TextRun, Window, canvas, div, fill, linear_color_stop, linear_gradient, outline, point, px, size,
};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::charts::format::Unit;
use crate::data::history::Bucket;
use crate::ui::theme::{Theme, theme};

pub const GUTTER: Pixels = px(56.);
const AXIS_HEIGHT: Pixels = px(16.);
const TOP_PAD: Pixels = px(6.);
const LABEL_LINE: Pixels = px(12.);
const TIME_STEPS: [i64; 13] = [
    60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400, 172800,
];

/// Fritsch–Carlson tangents: the cubic through these points never overshoots its neighbours.
pub fn tangents(p: &[(f32, f32)]) -> Vec<f32> {
    let n = p.len();
    if n < 2 {
        return vec![0.; n];
    }
    let d: Vec<f32> = p
        .windows(2)
        .map(|w| {
            let h = w[1].0 - w[0].0;
            if h > 0. { (w[1].1 - w[0].1) / h } else { 0. }
        })
        .collect();
    let mut m = vec![0.; n];
    m[0] = d[0];
    m[n - 1] = d[n - 2];
    for k in 1..n - 1 {
        m[k] = if d[k - 1] * d[k] <= 0. {
            0.
        } else {
            (d[k - 1] + d[k]) / 2.
        };
    }
    for k in 0..n - 1 {
        if d[k] == 0. {
            m[k] = 0.;
            m[k + 1] = 0.;
            continue;
        }
        let (a, b) = (m[k] / d[k], m[k + 1] / d[k]);
        let s = a * a + b * b;
        if s > 9. {
            let tau = 3. / s.sqrt();
            m[k] = tau * a * d[k];
            m[k + 1] = tau * b * d[k];
        }
    }
    m
}

/// Cubic Bézier segments `[control 1, control 2, end]` through `p`, starting from `p[0]`.
pub fn monotone(p: &[(f32, f32)]) -> Vec<[(f32, f32); 3]> {
    let m = tangents(p);
    p.windows(2)
        .enumerate()
        .map(|(k, w)| {
            let ((x0, y0), (x1, y1)) = (w[0], w[1]);
            let h = (x1 - x0) / 3.;
            [(x0 + h, y0 + m[k] * h), (x1 - h, y1 - m[k + 1] * h), (x1, y1)]
        })
        .collect()
}

/// Index ranges of buckets with no step wider than `max_step` seconds; a wider step is missing data.
pub fn runs(ts: &[i64], max_step: f64) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..=ts.len() {
        if i == ts.len() || (ts[i] - ts[i - 1]) as f64 > max_step {
            if i > start {
                out.push(start..i);
            }
            start = i;
        }
    }
    out
}

/// 2–6 round ticks (steps of 1, 2, 2.5 or 5 × 10ⁿ) covering `[lo, hi]`.
pub fn nice_ticks(lo: f64, hi: f64) -> Vec<f64> {
    let hi = if hi > lo { hi } else { lo + 1. };
    let mut mag = 10f64.powf(((hi - lo) / 5.).log10().floor() - 1.);
    loop {
        for m in [1., 2., 2.5, 5.] {
            let step = m * mag;
            let (first, last) = ((lo / step + 1e-9).floor(), (hi / step - 1e-9).ceil());
            if last - first <= 5. {
                return (first as i64..=last as i64).map(|i| i as f64 * step).collect();
            }
        }
        mag *= 10.;
    }
}

/// Clock-aligned ticks: at most `max` of them, on whole local minutes or hours.
pub fn time_ticks(from: f64, to: f64, max: usize, offset_s: i64) -> Vec<f64> {
    let span = to - from;
    let step = TIME_STEPS
        .iter()
        .copied()
        .find(|&s| span / s as f64 <= max as f64)
        .unwrap_or(TIME_STEPS[TIME_STEPS.len() - 1]);
    let first = (from.ceil() as i64 + offset_s + step - 1).div_euclid(step) * step - offset_s;
    (0..)
        .map(|i| (first + i * step) as f64)
        .take_while(|&t| t <= to)
        .collect()
}

fn local_offset(t: f64) -> i64 {
    Timestamp::from_second(t as i64).map_or(0, |ts| i64::from(TimeZone::system().to_offset(ts).seconds()))
}

pub fn clock(t: f64, pattern: &str) -> String {
    Timestamp::from_second(t as i64)
        .map(|ts| ts.to_zoned(TimeZone::system()).strftime(pattern).to_string())
        .unwrap_or_default()
}

/// Seconds before now for each label, now included, at most `max` of them on a round step.
pub fn relative_ticks(span: f64, max: usize) -> Vec<f64> {
    let step = TIME_STEPS
        .iter()
        .copied()
        .find(|&s| span / s as f64 <= max as f64 - 1.)
        .unwrap_or(TIME_STEPS[TIME_STEPS.len() - 1]) as f64;
    (0..).map(|i| i as f64 * step).take_while(|&ago| ago <= span).collect()
}

/// `now`, `10m`, `3h`, `2d`.
pub fn ago_label(ago: f64) -> String {
    let s = ago.round() as i64;
    if s == 0 {
        "now".into()
    } else if s % 86400 == 0 {
        format!("{}d", s / 86400)
    } else if s % 3600 == 0 {
        format!("{}h", s / 3600)
    } else {
        format!("{}m", s / 60)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum YScale {
    Fixed(f64, f64),
    /// From zero to a round number above the data.
    Zero,
    /// Around the data, at least this tall.
    Fit(f64),
}

fn scaled_ticks(unit: Unit, lo: f64, hi: f64) -> Vec<f64> {
    let s = unit.tick_scale(hi.abs().max(lo.abs()));
    nice_ticks(lo / s, hi / s).into_iter().map(|v| v * s).collect()
}

fn domain(y: YScale, unit: Unit, lo: f64, hi: f64) -> (f64, f64, Vec<f64>) {
    let ticks = match y {
        YScale::Fixed(a, b) => {
            let ticks = scaled_ticks(unit, a, b)
                .into_iter()
                .filter(|v| *v >= a && *v <= b)
                .collect();
            return (a, b, ticks);
        }
        YScale::Zero => {
            // Percent ticks are whole numbers, so an idle percent axis still needs room for distinct labels.
            let floor = match unit {
                Unit::Bytes | Unit::BytesPerSec => 1024.,
                Unit::Percent => 10.,
                _ => 1.,
            };
            scaled_ticks(unit, 0., if hi.is_finite() { hi.max(floor) } else { floor })
        }
        YScale::Fit(min_span) => {
            let (lo, hi) = if lo.is_finite() { (lo, hi) } else { (0., min_span) };
            let pad = ((min_span - (hi - lo)) / 2.).max(0.);
            scaled_ticks(unit, lo - pad, hi + pad)
        }
    };
    (ticks[0], ticks[ticks.len() - 1], ticks)
}

pub struct Line {
    pub data: Rc<[Bucket]>,
    pub color: Hsla,
    pub label: SharedString,
    pub dashed: bool,
}

struct Rule {
    value: f64,
    label: SharedString,
    color: Hsla,
}

/// What a chart keeps between frames: built paths and the hover cursor (a time, so charts can share it).
#[derive(Default)]
pub struct ChartState {
    built: Option<Built>,
    cursor: Rc<Cell<Option<f64>>>,
    pin: Option<Rc<Cell<Option<f64>>>>,
    hovering: bool,
}

impl ChartState {
    pub fn new() -> Rc<RefCell<Self>> {
        Rc::default()
    }

    /// Charts given the same cursor show one shared vertical rule.
    pub fn with_cursor(cursor: Rc<Cell<Option<f64>>>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            cursor,
            ..Self::default()
        }))
    }

    /// As `with_cursor`, but while `pin` holds a time the rule stays there; hovering still updates `cursor`.
    pub fn with_pin(cursor: Rc<Cell<Option<f64>>>, pin: Rc<Cell<Option<f64>>>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            cursor,
            pin: Some(pin),
            ..Self::default()
        }))
    }
}

struct Built {
    key: (u64, Bounds<Pixels>),
    plot: Bounds<Pixels>,
    under: Vec<(Path<Pixels>, Background)>,
    over: Vec<(Path<Pixels>, Background)>,
    dots: Vec<(Point<Pixels>, Hsla)>,
    rule_labels: Vec<(Pixels, SharedString, Hsla)>,
    y_ticks: Vec<(Pixels, SharedString)>,
    x_ticks: Vec<(Pixels, SharedString)>,
    lo: f64,
    hi: f64,
}

#[derive(IntoElement)]
pub struct TrendChart {
    state: Rc<RefCell<ChartState>>,
    version: u64,
    from: f64,
    to: f64,
    width_s: f64,
    gap_s: f64,
    lines: Vec<Line>,
    rules: Vec<Rule>,
    unit: Unit,
    y: YScale,
    fill: bool,
    relative: bool,
    height: Pixels,
}

impl TrendChart {
    /// `version` must change whenever the data or the time span does; it keys the path cache.
    pub fn new(state: Rc<RefCell<ChartState>>, version: u64, span: (f64, f64), width_s: f64, gap_s: f64) -> Self {
        Self {
            state,
            version,
            from: span.0,
            to: span.1,
            width_s,
            gap_s,
            lines: Vec::new(),
            rules: Vec::new(),
            unit: Unit::Percent,
            y: YScale::Zero,
            fill: false,
            relative: false,
            height: px(150.),
        }
    }

    pub fn line(mut self, data: Rc<[Bucket]>, color: Hsla, label: impl Into<SharedString>) -> Self {
        self.lines.push(Line {
            data,
            color,
            label: label.into(),
            dashed: false,
        });
        self
    }

    /// A projection rather than a record: dashed, with no range band or fill.
    pub fn dashed(mut self, data: Rc<[Bucket]>, color: Hsla, label: impl Into<SharedString>) -> Self {
        self.lines.push(Line {
            data,
            color,
            label: label.into(),
            dashed: true,
        });
        self
    }

    pub fn rule(mut self, value: f64, label: impl Into<SharedString>, color: Hsla) -> Self {
        self.rules.push(Rule {
            value,
            label: label.into(),
            color,
        });
        self
    }

    pub fn unit(mut self, unit: Unit) -> Self {
        self.unit = unit;
        self
    }

    pub fn y(mut self, y: YScale) -> Self {
        self.y = y;
        self
    }

    pub fn fill(mut self) -> Self {
        self.fill = true;
        self
    }

    /// X labels count back from now ("30m 20m 10m now") instead of showing clock times.
    pub fn relative_axis(mut self) -> Self {
        self.relative = true;
        self
    }

    pub fn height(mut self, height: Pixels) -> Self {
        self.height = height;
        self
    }

    fn visible<'a>(&'a self, line: &'a Line) -> impl Iterator<Item = &'a Bucket> + 'a {
        line.data
            .iter()
            .filter(|b| b.t as f64 + self.width_s >= self.from && b.t as f64 <= self.to)
    }

    fn x_of(&self, plot: &Bounds<Pixels>, t: f64) -> Pixels {
        plot.left() + plot.size.width * ((t - self.from) / (self.to - self.from).max(1.)) as f32
    }

    /// A bucket is drawn at its middle, except the newest, which may still be filling.
    fn center(&self, b: &Bucket) -> f64 {
        (b.t as f64 + self.width_s / 2.).min(self.to)
    }

    fn build(&self, bounds: Bounds<Pixels>) -> Built {
        let plot = Bounds::from_corners(
            point(bounds.left() + GUTTER, bounds.top() + TOP_PAD),
            point(bounds.right() - px(1.), bounds.bottom() - AXIS_HEIGHT),
        );
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for line in &self.lines {
            for b in self.visible(line) {
                lo = lo.min(b.min);
                hi = hi.max(b.max);
            }
        }
        let (lo, hi, ticks) = domain(self.y, self.unit, lo, hi);
        let span = (hi - lo).max(f64::EPSILON);
        let y_of = |v: f64| {
            let f = ((v - lo) / span).clamp(-0.02, 1.02) as f32;
            f32::from(plot.bottom()) - f32::from(plot.size.height) * f
        };
        let mut built = Built {
            key: (self.version, bounds),
            plot,
            under: Vec::new(),
            over: Vec::new(),
            dots: Vec::new(),
            rule_labels: Vec::new(),
            y_ticks: ticks
                .iter()
                .map(|&v| (px(y_of(v)), self.unit.format(v).into()))
                .collect(),
            x_ticks: Vec::new(),
            lo,
            hi,
        };
        let max_ticks = (f32::from(plot.size.width) / 90.).max(3.) as usize;
        let pattern = if self.to - self.from > 2. * 86400. {
            "%b %-d"
        } else {
            "%H:%M"
        };
        built.x_ticks = if self.relative {
            relative_ticks(self.to - self.from, (f32::from(plot.size.width) / 60.).max(3.) as usize)
                .into_iter()
                .map(|ago| (self.x_of(&plot, self.to - ago), ago_label(ago).into()))
                .collect()
        } else {
            time_ticks(self.from, self.to, max_ticks, local_offset(self.to))
                .into_iter()
                .map(|tick| (self.x_of(&plot, tick), clock(tick, pattern).into()))
                .collect()
        };

        for line in &self.lines {
            let buckets: Vec<&Bucket> = self.visible(line).collect();
            let ts: Vec<i64> = buckets.iter().map(|b| b.t).collect();
            let x: Vec<f32> = buckets
                .iter()
                .map(|b| f32::from(self.x_of(&plot, self.center(b))))
                .collect();
            for run in runs(&ts, self.gap_s) {
                let at = |f: fn(&Bucket) -> f64| -> Vec<(f32, f32)> {
                    run.clone().map(|i| (x[i], y_of(f(buckets[i])))).collect()
                };
                let avg = at(|b| b.avg);
                if avg.len() == 1 {
                    built.dots.push((point(px(avg[0].0), px(avg[0].1)), line.color));
                    continue;
                }
                if line.dashed {
                    let mut stroke = PathBuilder::stroke(px(1.5)).dash_array(&[px(5.), px(4.)]);
                    trace(&mut stroke, &avg, true);
                    if let Ok(path) = stroke.build() {
                        built.over.push((path, line.color.opacity(0.85).into()));
                    }
                    continue;
                }
                let mut lower = at(|b| b.min);
                lower.reverse();
                // Straight and faint, so the recorded range reads as backdrop to the bold mean.
                let mut band = PathBuilder::fill();
                for (i, &(x, y)) in at(|b| b.max).iter().chain(&lower).enumerate() {
                    if i == 0 {
                        band.move_to(point(px(x), px(y)));
                    } else {
                        band.line_to(point(px(x), px(y)));
                    }
                }
                band.close();
                if let Ok(path) = band.build() {
                    built.under.push((path, line.color.opacity(0.1).into()));
                }
                if self.fill {
                    let mut area = PathBuilder::fill();
                    trace(&mut area, &avg, true);
                    area.line_to(point(px(avg[avg.len() - 1].0), plot.bottom()));
                    area.line_to(point(px(avg[0].0), plot.bottom()));
                    area.close();
                    if let Ok(path) = area.build() {
                        let gradient = linear_gradient(
                            180.,
                            linear_color_stop(line.color.opacity(0.42), 0.),
                            linear_color_stop(line.color.opacity(0.04), 1.),
                        );
                        built.under.push((path, gradient));
                    }
                }
                let mut stroke = PathBuilder::stroke(px(2.));
                trace(&mut stroke, &avg, true);
                if let Ok(path) = stroke.build() {
                    built.over.push((path, line.color.into()));
                }
            }
        }

        for rule in self.rules.iter().filter(|r| r.value >= lo && r.value <= hi) {
            let y = px(y_of(rule.value));
            let mut dash = PathBuilder::stroke(px(1.)).dash_array(&[px(4.), px(3.)]);
            dash.move_to(point(plot.left(), y));
            dash.line_to(point(plot.right(), y));
            if let Ok(path) = dash.build() {
                built.over.push((path, rule.color.opacity(0.4).into()));
            }
            built.rule_labels.push((y, rule.label.clone(), rule.color));
        }
        built
    }
}

fn trace(path: &mut PathBuilder, p: &[(f32, f32)], start: bool) {
    let at = |(x, y): (f32, f32)| point(px(x), px(y));
    if start {
        path.move_to(at(p[0]));
    } else {
        path.line_to(at(p[0]));
    }
    for [c1, c2, end] in monotone(p) {
        path.cubic_bezier_to(at(end), at(c1), at(c2));
    }
}

/// Theme values copied out, since painting needs `&mut App`.
struct Ink {
    font: Font,
    text: Hsla,
    secondary: Hsla,
    grid: Hsla,
    border: Hsla,
    tooltip: Hsla,
    axis: Pixels,
    caption: Pixels,
}

impl Ink {
    fn new(t: &Theme) -> Self {
        let mut font = gpui::font(t.font.clone());
        font.features = t.numbers.clone();
        Self {
            font,
            text: t.text,
            secondary: t.text_secondary,
            grid: t.text_secondary.opacity(0.15),
            border: t.text_secondary.opacity(0.22),
            tooltip: t.tooltip,
            axis: t.axis,
            caption: t.caption,
        }
    }
}

pub(crate) fn shape(
    window: &mut Window,
    font: &Font,
    text: SharedString,
    size: Pixels,
    color: Hsla,
) -> gpui::ShapedLine {
    let run = TextRun {
        len: text.len(),
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window.text_system().shape_line(text, size, &[run], None)
}

fn paint_text(window: &mut Window, cx: &mut App, line: &gpui::ShapedLine, origin: Point<Pixels>) {
    line.paint(origin, LABEL_LINE, TextAlign::Left, None, window, cx).ok();
}

impl TrendChart {
    fn paint(self, bounds: Bounds<Pixels>, hitbox: Hitbox, window: &mut Window, cx: &mut App) {
        let ink = Ink::new(theme(cx));
        let mut state = self.state.borrow_mut();
        if state.built.as_ref().is_none_or(|b| b.key != (self.version, bounds)) {
            state.built = Some(self.build(bounds));
        }
        let built = state.built.as_ref().expect("built above");
        let plot = built.plot;

        for (y, _) in &built.y_ticks {
            window.paint_quad(fill(
                Bounds::new(point(plot.left(), *y - px(0.25)), size(plot.size.width, px(0.5))),
                ink.grid,
            ));
        }
        for (x, _) in &built.x_ticks {
            window.paint_quad(fill(
                Bounds::new(point(*x - px(0.25), plot.top()), size(px(0.5), plot.size.height)),
                ink.grid,
            ));
        }
        window.with_content_mask(Some(ContentMask { bounds: plot }), |window| {
            for (path, bg) in built.under.iter().chain(&built.over) {
                window.paint_path(path.clone(), *bg);
            }
            for (at, color) in &built.dots {
                window.paint_quad(dot(*at, px(1.5), *color));
            }
        });
        window.paint_quad(outline(plot, ink.border, BorderStyle::Solid).border_widths(px(0.5)));

        for (y, label) in &built.y_ticks {
            let line = shape(window, &ink.font, label.clone(), ink.axis, ink.secondary);
            let origin = point(plot.left() - px(6.) - line.width, *y - LABEL_LINE / 2.);
            paint_text(window, cx, &line, origin);
        }
        for (x, label) in &built.x_ticks {
            let line = shape(window, &ink.font, label.clone(), ink.axis, ink.secondary);
            let origin = point(*x - line.width / 2., plot.bottom() + px(3.));
            paint_text(window, cx, &line, origin);
        }
        for (y, label, color) in &built.rule_labels {
            let line = shape(window, &ink.font, label.clone(), ink.axis, *color);
            paint_text(window, cx, &line, point(plot.left() + px(4.), *y - LABEL_LINE / 2.));
        }

        let pinned = state.pin.as_ref().and_then(|p| p.get());
        // A pin marks every chart in the group; only the chart under the pointer adds a tooltip to it.
        let tooltip = pinned.is_none() || state.hovering;
        if let Some(at) = pinned
            .or(state.cursor.get())
            .filter(|t| *t >= self.from && *t <= self.to)
        {
            self.paint_cursor(at, built, &ink, tooltip, window, cx);
        }
        drop(state);

        let state = self.state.clone();
        let (from, to) = (self.from, self.to);
        window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, _| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let mut state = state.borrow_mut();
            if hitbox.is_hovered(window) && plot.contains(&e.position) {
                let f = f64::from((e.position.x - plot.left()) / plot.size.width);
                state.cursor.set(Some(from + f * (to - from)));
                state.hovering = true;
                window.refresh();
            } else if state.hovering {
                state.hovering = false;
                state.cursor.set(None);
                window.refresh();
            }
        });
    }

    fn paint_cursor(&self, at: f64, built: &Built, ink: &Ink, tooltip: bool, window: &mut Window, cx: &mut App) {
        let plot = built.plot;
        let x = self.x_of(&plot, at);
        window.paint_quad(fill(
            Bounds::new(point(x - px(0.5), plot.top()), size(px(1.), plot.size.height)),
            ink.secondary.opacity(0.6),
        ));
        let pattern = if self.to - self.from > 6. * 3600. {
            "%a %H:%M"
        } else {
            "%H:%M:%S"
        };
        let mut rows = vec![(None, SharedString::from(clock(at, pattern)))];
        let span = (built.hi - built.lo).max(f64::EPSILON);
        let reach = self.gap_s.max(self.width_s) / 2. + 1.;
        for line in &self.lines {
            let near = line
                .data
                .iter()
                .min_by(|a, b| (self.center(a) - at).abs().total_cmp(&(self.center(b) - at).abs()));
            let value = match near.filter(|b| (self.center(b) - at).abs() <= reach) {
                Some(b) => {
                    let y = plot.bottom() - plot.size.height * ((b.avg - built.lo) / span).clamp(0., 1.) as f32;
                    window.paint_quad(dot(point(self.x_of(&plot, self.center(b)), y), px(3.), line.color));
                    self.unit.format(b.avg)
                }
                None => "no data".into(),
            };
            let label = if line.label.is_empty() {
                value
            } else {
                format!("{}  {value}", line.label)
            };
            rows.push((Some(line.color), label.into()));
        }
        if !tooltip {
            return;
        }

        let shaped: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(i, (_, text))| {
                let color = if i == 0 { ink.secondary } else { ink.text };
                shape(window, &ink.font, text.clone(), ink.caption, color)
            })
            .collect();
        let row_h = px(15.);
        let pad = px(8.);
        let text_w = shaped.iter().map(|l| l.width).fold(px(0.), |a, b| a.max(b));
        let box_size = size(text_w + pad * 2. + px(12.), row_h * shaped.len() as f32 + pad * 2.);
        let left = if x + px(10.) + box_size.width > plot.right() {
            x - px(10.) - box_size.width
        } else {
            x + px(10.)
        };
        let tip = Bounds::new(point(left, plot.top() + px(4.)), box_size);
        window.paint_quad(
            fill(tip, ink.tooltip)
                .corner_radii(px(6.))
                .border_widths(px(0.5))
                .border_color(ink.border),
        );
        for (i, ((color, _), line)) in rows.iter().zip(&shaped).enumerate() {
            let y = tip.top() + pad + row_h * i as f32;
            let mut x = tip.left() + pad;
            if let Some(color) = color {
                window.paint_quad(dot(point(x + px(3.), y + row_h / 2.), px(3.), *color));
                x += px(12.);
            }
            line.paint(point(x, y), row_h, TextAlign::Left, None, window, cx).ok();
        }
    }
}

pub(crate) fn dot(center: Point<Pixels>, r: Pixels, color: Hsla) -> gpui::PaintQuad {
    fill(Bounds::centered_at(center, size(r * 2., r * 2.)), color).corner_radii(r)
}

impl RenderOnce for TrendChart {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let height = self.height;
        div().w_full().h(height).child(
            canvas(
                |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                move |bounds, hitbox, window, cx| self.paint(bounds, hitbox, window, cx),
            )
            .size_full(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random values, so a failure reproduces.
    fn noise(seed: &mut u64) -> f32 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*seed >> 40) as f32 / (1u64 << 24) as f32
    }

    fn bezier(p0: (f32, f32), [c1, c2, p1]: [(f32, f32); 3], t: f32) -> (f32, f32) {
        let u = 1. - t;
        let f =
            |a: f32, b: f32, c: f32, d: f32| u * u * u * a + 3. * u * u * t * b + 3. * u * t * t * c + t * t * t * d;
        (f(p0.0, c1.0, c2.0, p1.0), f(p0.1, c1.1, c2.1, p1.1))
    }

    #[test]
    fn the_curve_never_leaves_the_range_of_the_two_points_it_joins() {
        let mut seed = 7;
        for case in 0..400 {
            let n = 2 + case % 30;
            let mut x = 0.;
            let p: Vec<(f32, f32)> = (0..n)
                .map(|i| {
                    x += 0.5 + noise(&mut seed) * 20.;
                    // Every fourth case has plateaus and spikes, where natural splines overshoot most.
                    let y = if case % 4 == 0 && i % 3 != 0 {
                        50.
                    } else {
                        noise(&mut seed) * 100.
                    };
                    (x, y)
                })
                .collect();
            for (k, seg) in monotone(&p).into_iter().enumerate() {
                let (lo, hi) = (p[k].1.min(p[k + 1].1), p[k].1.max(p[k + 1].1));
                for s in 0..=32 {
                    let (_, y) = bezier(p[k], seg, s as f32 / 32.);
                    assert!(
                        y >= lo - 1e-3 && y <= hi + 1e-3,
                        "case {case}, segment {k}: {y} outside [{lo}, {hi}]"
                    );
                }
            }
        }
    }

    #[test]
    fn a_step_stays_flat_on_both_sides() {
        let p = [(0., 0.), (1., 0.), (2., 10.), (3., 10.)];
        let segs = monotone(&p);
        for s in 0..=10 {
            assert!(
                bezier(p[0], segs[0], s as f32 / 10.).1.abs() < 1e-4,
                "no dip before the step"
            );
            assert!(
                (bezier(p[2], segs[2], s as f32 / 10.).1 - 10.).abs() < 1e-4,
                "no bump after it"
            );
        }
    }

    #[test]
    fn missing_buckets_split_the_line() {
        assert_eq!(runs(&[], 15.), Vec::<Range<usize>>::new());
        assert_eq!(runs(&[0, 15, 30], 22.5), [Range { start: 0, end: 3 }]);
        assert_eq!(runs(&[0, 15, 60, 75, 90, 300], 22.5), [0..2, 2..5, 5..6]);
    }

    #[test]
    fn value_ticks_are_round_and_cover_the_data() {
        for (lo, hi) in [
            (0., 1.),
            (0., 7.3),
            (0., 100.),
            (0., 18.6),
            (45., 120.),
            (0., 95.4),
            (3., 3.),
            (0., 0.01),
        ] {
            let ticks = nice_ticks(lo, hi);
            assert!((2..=6).contains(&ticks.len()), "{lo}..{hi}: {ticks:?}");
            assert!(ticks[0] <= lo && *ticks.last().unwrap() >= hi, "{lo}..{hi}: {ticks:?}");
            let step = ticks[1] - ticks[0];
            let mantissa = step / 10f64.powf(step.log10().floor());
            assert!(
                [1., 2., 2.5, 5.].iter().any(|m| (m - mantissa).abs() < 1e-6),
                "{step} is not a round step"
            );
        }
        assert_eq!(nice_ticks(0., 100.), [0., 20., 40., 60., 80., 100.]);
        for hi in 1..2000 {
            let n = nice_ticks(0., f64::from(hi) / 10.).len();
            assert!((4..=6).contains(&n), "0..{}: {n} ticks", f64::from(hi) / 10.);
        }
    }

    #[test]
    fn time_ticks_land_on_round_local_minutes() {
        let from = 1_700_000_123.;
        let ticks = time_ticks(from, from + 1800., 6, 0);
        assert!(ticks.iter().all(|t| *t as i64 % 300 == 0), "{ticks:?}");
        assert_eq!(ticks.len(), 6);
        let shifted = time_ticks(from, from + 86400., 8, 3600);
        assert!(shifted.iter().all(|t| (*t as i64 + 3600) % 10800 == 0), "{shifted:?}");
    }

    #[test]
    fn relative_labels_count_back_from_now() {
        let labels: Vec<String> = relative_ticks(1800., 4).into_iter().map(ago_label).collect();
        assert_eq!(labels, ["now", "10m", "20m", "30m"]);
        let labels: Vec<String> = relative_ticks(7. * 86400., 5).into_iter().map(ago_label).collect();
        assert_eq!(labels, ["now", "2d", "4d", "6d"]);
    }

    #[test]
    fn a_zero_based_byte_axis_steps_in_whole_units_of_its_maximum() {
        let gib = 1024f64.powi(3);
        let (lo, hi, ticks) = domain(YScale::Zero, Unit::Bytes, 0., 18.6 * gib);
        assert_eq!(lo, 0.);
        assert!(hi >= 18.6 * gib);
        assert!(ticks.iter().all(|t| (t / gib).fract() == 0.), "{ticks:?}");
        let (_, _, ticks) = domain(YScale::Fixed(0., 4. * gib), Unit::Bytes, 0., 0.);
        assert_eq!(
            ticks,
            [0., gib, 2. * gib, 3. * gib, 4. * gib],
            "a fixed byte axis steps in whole GB too"
        );
        let (_, hi, _) = domain(YScale::Zero, Unit::BytesPerSec, f64::INFINITY, f64::NEG_INFINITY);
        assert_eq!(hi, 1024., "an empty chart still has an axis");
        let (lo, hi, _) = domain(YScale::Fit(10.), Unit::Celsius, 60., 61.);
        assert!(hi - lo >= 10.);
    }
}
