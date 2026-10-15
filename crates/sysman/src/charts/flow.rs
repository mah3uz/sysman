use std::collections::HashMap;
use std::time::Instant;

use gpui::{
    App, BorderStyle, Bounds, Entity, Font, Hsla, IntoElement, ParentElement, PathBuilder, Pixels, Point, RenderOnce,
    SharedString, Styled, TextAlign, TransformationMatrix, Window, canvas, div, fill, outline, point, px, size,
};
use gpui_kit_assets::IconName;

use crate::charts::trend::{dot, shape};
use crate::ui::shell::paused;
use crate::ui::theme::theme;

pub const HEIGHT: Pixels = px(240.);
/// Dot speed along a line, px/s, for an idle and for a saturated flow.
pub const SPEED: (f64, f64) = (40., 90.);
/// Share of dot slots lit for an idle and for a saturated flow; a heavier flow shows more dots, not faster ones.
const DENSITY: (f64, f64) = (0.16, 0.72);
/// Distance between dot slots along a line.
const PITCH: f64 = 26.;
/// A new reading eases the speed and density over this long.
const EASE_S: f64 = 0.4;
/// A longer gap between frames (paused, hidden) does not move the dots on.
const MAX_FRAME_S: f64 = 0.25;
/// Dots fade in and out over this distance at each end of a line.
const FADE_PX: f64 = 14.;
/// Watts at which a flow counts as half saturated.
const HALF_W: f64 = 25.;
const LEFT_X: f32 = 56.;
const SINK_WIDTH: f32 = 132.;
const HUB_SIZE: (f32, f32) = (86., 64.);
const STACKED_HUB_SIZE: (f32, f32) = (132., 42.);
const LINE: Pixels = px(14.);

/// A left-column node: an energy source such as the wall or the battery.
pub enum Source {
    Plug {
        caption: SharedString,
        color: Hsla,
    },
    Battery {
        percent: f64,
        fill: Hsla,
        caption: SharedString,
        caption_color: Hsla,
    },
}

/// A centre-column node the energy passes through: the machine, or a component.
pub struct Hub {
    pub icon: IconName,
    pub title: Option<SharedString>,
    pub value: SharedString,
}

/// A right-column node the energy ends in.
pub struct Sink {
    pub name: SharedString,
    pub detail: SharedString,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum End {
    Source(usize),
    /// A hub's left edge, nudged up or down so two lines into one hub stay apart.
    HubIn(usize, f32),
    HubOut(usize),
    Sink(usize),
}

pub struct Link {
    pub from: End,
    pub to: End,
    pub color: Hsla,
    /// Sets how many dots travel the line and how fast; `None` draws the line without dots.
    pub watts: Option<f64>,
}

/// Lines from sources through hubs to sinks, with dots travelling each line more densely the more watts it carries.
#[derive(IntoElement)]
pub struct EnergyFlow {
    pub sources: Vec<Source>,
    pub hubs: Vec<Hub>,
    pub sinks: Vec<Sink>,
    pub links: Vec<Link>,
    pub animate: bool,
}

/// Evenly spread rows between fractions `top` and `bottom` of `height`; a single row sits in the middle.
pub fn rows(count: usize, height: f32, top: f32, bottom: f32) -> Vec<f32> {
    match count {
        0 => Vec::new(),
        1 => vec![height / 2.],
        n => {
            let step = (bottom - top) * height / (n - 1) as f32;
            (0..n).map(|i| top * height + step * i as f32).collect()
        }
    }
}

/// 0 for an idle line, approaching 1 for a heavy one; it saturates so a big flow reads as more, not frantic.
pub fn level(watts: f64) -> f64 {
    let w = watts.max(0.);
    w / (w + HALF_W)
}

fn smoothstep(u: f64) -> f64 {
    let u = u.clamp(0., 1.);
    u * u * (3. - 2. * u)
}

/// A line's level easing from `x0` to `x1` since `t0`, and the distance its dots had travelled at `t0`.
/// Travel is the closed-form integral of the eased speed, so it never jumps when a new reading arrives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Drift {
    t0: f64,
    d0: f64,
    x0: f64,
    x1: f64,
}

impl Drift {
    pub fn new(t: f64, x: f64) -> Self {
        Self {
            t0: t,
            d0: 0.,
            x0: x,
            x1: x,
        }
    }

    pub fn level(&self, t: f64) -> f64 {
        self.x0 + (self.x1 - self.x0) * smoothstep((t - self.t0) / EASE_S)
    }

    pub fn speed(&self, t: f64) -> f64 {
        SPEED.0 + (SPEED.1 - SPEED.0) * self.level(t)
    }

    /// Distance along the line, px.
    pub fn travel(&self, t: f64) -> f64 {
        let tau = (t - self.t0).max(0.);
        let u = tau / EASE_S;
        // the integral of smoothstep over the ease, then 1 per second after it
        let eased = if u < 1. {
            EASE_S * (u.powi(3) - u.powi(4) / 2.)
        } else {
            EASE_S * 0.5 + (tau - EASE_S)
        };
        let level_area = self.x0 * tau + (self.x1 - self.x0) * eased;
        self.d0 + SPEED.0 * tau + (SPEED.1 - SPEED.0) * level_area
    }

    pub fn retarget(&mut self, t: f64, x: f64) {
        if (x - self.x1).abs() > 1e-6 {
            *self = Self {
                t0: t,
                d0: self.travel(t),
                x0: self.level(t),
                x1: x,
            };
        }
    }
}

/// Whether dot slot `n` shows at `density`, from 0 to 1. Golden-ratio steps spread the lit slots evenly at
/// any density, and a slot fades rather than pops as the density passes it.
pub fn lit(n: i64, seed: f64, density: f64) -> f64 {
    let u = (n as f64 * 0.618_033_988_75 + seed).rem_euclid(1.);
    ((density - u) / 0.08).clamp(0., 1.)
}

/// The animation clock and each line's drift, kept across frames.
#[derive(Default)]
struct Motion {
    /// Seconds of motion shown so far; it stands still while no frames are drawn.
    clock: f64,
    last: Option<Instant>,
    lines: HashMap<String, Drift>,
}

impl Motion {
    fn advance(&mut self, now: Instant, animate: bool) {
        if animate && let Some(last) = self.last {
            self.clock += now.duration_since(last).as_secs_f64().min(MAX_FRAME_S);
        }
        self.last = Some(now);
    }
}

fn line_key(link: &Link) -> String {
    format!("{:?}>{:?}", link.from, link.to)
}

fn seed(key: &str) -> f64 {
    let h = key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    (h % 10_000) as f64 / 10_000.
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Conduit {
    pub a: (f32, f32),
    pub c1: (f32, f32),
    pub c2: (f32, f32),
    pub b: (f32, f32),
}

impl Conduit {
    /// Leaves and arrives horizontally, bending halfway.
    pub fn between(a: (f32, f32), b: (f32, f32)) -> Self {
        let dx = (b.0 - a.0) * 0.5;
        Self {
            a,
            c1: (a.0 + dx, a.1),
            c2: (b.0 - dx, b.1),
            b,
        }
    }

    pub fn at(&self, t: f32) -> (f32, f32) {
        let u = 1. - t;
        let w = [u * u * u, 3. * u * u * t, 3. * u * t * t, t * t * t];
        let p = [self.a, self.c1, self.c2, self.b];
        (0..4).fold((0., 0.), |acc, i| (acc.0 + w[i] * p[i].0, acc.1 + w[i] * p[i].1))
    }

    /// A table of the curve's length against its parameter, built once per frame.
    pub fn track(self) -> Track {
        const N: usize = 64;
        let mut lengths = Vec::with_capacity(N + 1);
        let mut total = 0.;
        let mut prev = self.a;
        for i in 0..=N {
            let p = self.at(i as f32 / N as f32);
            total += dist(prev, p);
            lengths.push(total);
            prev = p;
        }
        Track { curve: self, lengths }
    }
}

/// A conduit measured by arc length, so dots placed at equal distances move at constant speed through the bends.
pub struct Track {
    curve: Conduit,
    lengths: Vec<f32>,
}

impl Track {
    pub fn length(&self) -> f32 {
        self.lengths.last().copied().unwrap_or(0.)
    }

    /// The point `d` px along the curve.
    pub fn at_distance(&self, d: f32) -> (f32, f32) {
        let d = d.clamp(0., self.length());
        let i = self
            .lengths
            .partition_point(|&l| l < d)
            .clamp(1, self.lengths.len() - 1);
        let (l0, l1) = (self.lengths[i - 1], self.lengths[i]);
        let k = if l1 > l0 { (d - l0) / (l1 - l0) } else { 0. };
        let n = (self.lengths.len() - 1) as f32;
        self.curve.at((i as f32 - 1. + k) / n)
    }
}

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt()
}

struct Geometry {
    sources: Vec<(f32, f32)>,
    hubs: Vec<(f32, f32)>,
    hub_size: (f32, f32),
    sinks: Vec<(f32, f32)>,
}

impl Geometry {
    fn new(flow: &EnergyFlow, w: f32, h: f32) -> Self {
        let mid = h / 2.;
        let sources = match flow.sources.len() {
            2 => vec![(LEFT_X, mid - h * 0.26), (LEFT_X, mid + h * 0.24)],
            n => rows(n, h, 0.25, 0.75).into_iter().map(|y| (LEFT_X, y)).collect(),
        };
        let hub_size = if flow.hubs.len() > 1 {
            STACKED_HUB_SIZE
        } else {
            HUB_SIZE
        };
        let hubs = rows(flow.hubs.len(), h, 0.2, 0.8)
            .into_iter()
            .map(|y| (w * 0.46, y))
            .collect();
        let sink_x = w - SINK_WIDTH - 8.;
        let sinks = rows(flow.sinks.len(), h, 0.12, 0.88)
            .into_iter()
            .map(|y| (sink_x, y))
            .collect();
        Self {
            sources,
            hubs,
            hub_size,
            sinks,
        }
    }

    fn anchor(&self, end: End) -> Option<(f32, f32)> {
        let (hw, _) = self.hub_size;
        Some(match end {
            End::Source(i) => {
                let (x, y) = *self.sources.get(i)?;
                (x + 28., y)
            }
            End::HubIn(i, dy) => {
                let (x, y) = *self.hubs.get(i)?;
                (x - hw / 2., y + dy)
            }
            End::HubOut(i) => {
                let (x, y) = *self.hubs.get(i)?;
                (x + hw / 2., y)
            }
            End::Sink(i) => *self.sinks.get(i)?,
        })
    }
}

struct Ink {
    font: Font,
    bold: Font,
    text: Hsla,
    secondary: Hsla,
    box_fill: Hsla,
    box_line: Hsla,
    caption: Pixels,
    small: Pixels,
    title: Pixels,
    accent: Hsla,
}

fn text(
    window: &mut Window,
    cx: &mut App,
    font: &Font,
    s: SharedString,
    (size, color): (Pixels, Hsla),
    at: Point<Pixels>,
    align: TextAlign,
) {
    let line = shape(window, font, s, size, color);
    let x = match align {
        TextAlign::Center => at.x - line.width / 2.,
        TextAlign::Right => at.x - line.width,
        TextAlign::Left => at.x,
    };
    line.paint(point(x, at.y - LINE / 2.), LINE, TextAlign::Left, None, window, cx)
        .ok();
}

fn icon(window: &mut Window, cx: &App, name: IconName, center: Point<Pixels>, side: Pixels, color: Hsla) {
    let bounds = Bounds::centered_at(center, size(side, side));
    window
        .paint_svg(bounds, name.path(), None, TransformationMatrix::unit(), color, cx)
        .ok();
}

fn p(xy: (f32, f32)) -> Point<Pixels> {
    point(px(xy.0), px(xy.1))
}

impl EnergyFlow {
    fn paint(&self, bounds: Bounds<Pixels>, motion: &Entity<Motion>, window: &mut Window, cx: &mut App) {
        let t = theme(cx);
        let mut font = gpui::font(t.font.clone());
        font.features = t.numbers.clone();
        let mut bold = font.clone();
        bold.weight = gpui::FontWeight::SEMIBOLD;
        let ink = Ink {
            font,
            bold,
            text: t.text,
            secondary: t.text_secondary,
            box_fill: gpui::white().opacity(0.07),
            box_line: gpui::white().opacity(0.12),
            caption: t.caption,
            small: t.small_caption,
            title: t.panel_title,
            accent: t.orange,
        };
        let (ox, oy) = (f32::from(bounds.left()), f32::from(bounds.top()));
        let g = Geometry::new(self, f32::from(bounds.size.width), f32::from(bounds.size.height));
        let shift = |(x, y): (f32, f32)| (x + ox, y + oy);

        let conduits: Vec<(Conduit, &Link)> = self
            .links
            .iter()
            .filter_map(|l| Some((Conduit::between(shift(g.anchor(l.from)?), shift(g.anchor(l.to)?)), l)))
            .collect();
        for (c, link) in &conduits {
            let mut path = PathBuilder::stroke(px(1.75));
            path.move_to(p(c.a));
            path.cubic_bezier_to(p(c.b), p(c.c1), p(c.c2));
            if let Ok(path) = path.build() {
                window.paint_path(path, link.color.opacity(0.4));
            }
        }

        for (source, &at) in self.sources.iter().zip(&g.sources) {
            let at = shift(at);
            match source {
                Source::Plug { caption, color } => {
                    icon(window, cx, IconName::Plug, p((at.0, at.1 - 9.)), px(20.), *color);
                    text(
                        window,
                        cx,
                        &ink.font,
                        caption.clone(),
                        (ink.small, ink.secondary),
                        p((at.0, at.1 + 13.)),
                        TextAlign::Center,
                    );
                }
                Source::Battery {
                    percent,
                    fill: tint,
                    caption,
                    caption_color,
                } => {
                    battery_glyph(window, (at.0 - 23., at.1 - 19.), *percent, *tint, ink.secondary);
                    text(
                        window,
                        cx,
                        &ink.bold,
                        caption.clone(),
                        (ink.small, *caption_color),
                        p((at.0, at.1 + 13.)),
                        TextAlign::Center,
                    );
                }
            }
        }

        let (hw, hh) = g.hub_size;
        for (hub, &at) in self.hubs.iter().zip(&g.hubs) {
            let at = shift(at);
            let rect = Bounds::centered_at(p(at), size(px(hw), px(hh)));
            window.paint_quad(fill(rect, ink.box_fill).corner_radii(px(12.)));
            window.paint_quad(
                outline(rect, ink.box_line, BorderStyle::Solid)
                    .border_widths(px(0.5))
                    .corner_radii(px(12.)),
            );
            match &hub.title {
                None => {
                    icon(window, cx, hub.icon, p((at.0, at.1 - 11.)), px(26.), ink.text);
                    text(
                        window,
                        cx,
                        &ink.bold,
                        hub.value.clone(),
                        (ink.title, ink.text),
                        p((at.0, at.1 + 17.)),
                        TextAlign::Center,
                    );
                }
                Some(title) => {
                    let left = at.0 - hw / 2. + 12.;
                    icon(window, cx, hub.icon, p((left + 8., at.1)), px(16.), ink.secondary);
                    text(
                        window,
                        cx,
                        &ink.bold,
                        title.clone(),
                        (ink.caption, ink.text),
                        p((left + 24., at.1 - 7.)),
                        TextAlign::Left,
                    );
                    text(
                        window,
                        cx,
                        &ink.font,
                        hub.value.clone(),
                        (ink.small, ink.secondary),
                        p((left + 24., at.1 + 8.)),
                        TextAlign::Left,
                    );
                }
            }
        }

        for (sink, &at) in self.sinks.iter().zip(&g.sinks) {
            let at = shift(at);
            window.paint_quad(
                fill(
                    Bounds::centered_at(p((at.0 + 8., at.1)), size(px(14.), px(14.))),
                    ink.box_fill,
                )
                .corner_radii(px(4.)),
            );
            window.paint_quad(dot(p((at.0 + 8., at.1)), px(3.), ink.accent));
            let name = clip(&sink.name, 20);
            text(
                window,
                cx,
                &ink.bold,
                name,
                (ink.caption, ink.text),
                p((at.0 + 23., at.1 - 7.)),
                TextAlign::Left,
            );
            text(
                window,
                cx,
                &ink.font,
                sink.detail.clone(),
                (ink.small, ink.secondary),
                p((at.0 + 23., at.1 + 8.)),
                TextAlign::Left,
            );
        }

        let flowing = conduits.iter().any(|(_, l)| l.watts.is_some());
        let animate = self.animate && flowing && !paused(cx);
        let (clock, drifts) = motion.update(cx, |m, _| {
            m.advance(Instant::now(), animate);
            let clock = m.clock;
            let mut kept = HashMap::new();
            let drifts: Vec<Option<(Drift, String)>> = conduits
                .iter()
                .map(|(_, link)| {
                    let x = level(link.watts?);
                    let key = line_key(link);
                    let mut d = m.lines.get(&key).copied().unwrap_or_else(|| Drift::new(clock, x));
                    d.retarget(clock, x);
                    kept.insert(key.clone(), d);
                    Some((d, key))
                })
                .collect();
            m.lines = kept;
            (clock, drifts)
        });
        #[cfg(debug_assertions)]
        let mut dev_travel = Vec::new();
        for ((c, link), drift) in conduits.iter().zip(drifts) {
            let Some((drift, key)) = drift else {
                continue;
            };
            let track = c.track();
            let len = f64::from(track.length());
            let x = drift.level(clock);
            let travel = drift.travel(clock);
            #[cfg(debug_assertions)]
            dev_travel.push(travel);
            let density = DENSITY.0 + (DENSITY.1 - DENSITY.0) * x;
            let r = 2.4 + 1.4 * x;
            let seed = seed(&key);
            let first = ((travel - len) / PITCH).ceil() as i64;
            let last = (travel / PITCH).floor() as i64;
            for n in first..=last {
                let at = travel - n as f64 * PITCH;
                let alpha = lit(n, seed, density) * smoothstep(at.min(len - at) / FADE_PX);
                if alpha > 0.01 {
                    let centre = p(track.at_distance(at as f32));
                    window.paint_quad(dot(centre, px(r as f32), link.color.opacity(0.95 * alpha as f32)));
                }
            }
        }
        #[cfg(debug_assertions)]
        dev::record(&dev_travel);
        // Requested from a painted diagram only, so frames stop when its tab closes or the window hides.
        if animate {
            window.request_animation_frame();
        }
    }
}

fn clip(s: &str, max: usize) -> SharedString {
    if s.chars().count() <= max {
        return s.to_owned().into();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out.into()
}

fn battery_glyph(window: &mut Window, origin: (f32, f32), percent: f64, tint: Hsla, frame: Hsla) {
    let (w, h) = (46., 22.);
    let cap_w = w * 0.08;
    let body_w = w - cap_w - 1.;
    let body = Bounds::new(p(origin), size(px(body_w), px(h)));
    window.paint_quad(
        outline(body, frame, BorderStyle::Solid)
            .border_widths(px(1.2))
            .corner_radii(px(4.)),
    );
    let inset = 2.5;
    let fill_w = ((body_w - inset * 2.) * (percent / 100.).clamp(0., 1.) as f32).max(0.);
    window.paint_quad(
        fill(
            Bounds::new(
                p((origin.0 + inset, origin.1 + inset)),
                size(px(fill_w), px(h - inset * 2.)),
            ),
            tint,
        )
        .corner_radii(px(2.)),
    );
    window.paint_quad(
        fill(
            Bounds::new(
                p((origin.0 + body_w + 0.5, origin.1 + h * 0.3)),
                size(px(cap_w), px(h * 0.4)),
            ),
            frame,
        )
        .corner_radii(px(1.)),
    );
}

impl RenderOnce for EnergyFlow {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let motion = window.use_keyed_state("energy-flow-motion", cx, |_, _| Motion::default());
        div().w_full().h(HEIGHT).child(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, cx| self.paint(bounds, &motion, window, cx),
            )
            .size_full(),
        )
    }
}

/// Frame intervals and each line's dot step on stderr every 5 s while `SYSMAN_DEV_FRAMES` is set.
#[cfg(debug_assertions)]
mod dev {
    use std::cell::RefCell;
    use std::time::Instant;

    struct Stats {
        since: Instant,
        last: Option<(Instant, Vec<f64>)>,
        intervals: Vec<f64>,
        steps: Vec<Vec<f64>>,
    }

    thread_local! {
        static STATS: RefCell<Option<Stats>> = RefCell::new(std::env::var_os("SYSMAN_DEV_FRAMES").map(|_| Stats {
            since: Instant::now(),
            last: None,
            intervals: Vec::new(),
            steps: Vec::new(),
        }));
    }

    fn mean_sd(v: &[f64]) -> (f64, f64) {
        let n = v.len().max(1) as f64;
        let mean = v.iter().sum::<f64>() / n;
        (mean, (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n).sqrt())
    }

    /// `travel` is each line's distance along its path.
    pub fn record(travel: &[f64]) {
        STATS.with_borrow_mut(|stats| {
            let Some(s) = stats else {
                return;
            };
            let now = Instant::now();
            if let Some((at, before)) = &s.last
                && before.len() == travel.len()
            {
                s.intervals.push(now.duration_since(*at).as_secs_f64() * 1000.);
                s.steps.resize(travel.len(), Vec::new());
                for (i, (a, b)) in before.iter().zip(travel).enumerate() {
                    s.steps[i].push(b - a);
                }
            }
            s.last = Some((now, travel.to_vec()));
            if s.since.elapsed().as_secs_f64() >= 5. && !s.intervals.is_empty() {
                let (im, isd) = mean_sd(&s.intervals);
                let imax = s.intervals.iter().copied().fold(0., f64::max);
                let lines: Vec<String> = s
                    .steps
                    .iter()
                    .map(|v| {
                        let (m, sd) = mean_sd(v);
                        let max = v.iter().copied().fold(0., f64::max);
                        format!("{m:.2}±{sd:.2}({max:.1})")
                    })
                    .collect();
                eprintln!(
                    "SYSMAN_DEV_FRAMES flow: {} frames, interval {im:.1}±{isd:.1} ms (max {imax:.1}); step px mean±sd(max) per line: {}",
                    s.intervals.len(),
                    lines.join(" ")
                );
                s.since = now;
                s.intervals.clear();
                s.steps.clear();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_spread_between_the_margins_and_one_row_centres() {
        assert_eq!(rows(1, 240., 0.12, 0.88), [120.]);
        let r = rows(5, 200., 0.1, 0.9);
        assert!((r[0] - 20.).abs() < 1e-3);
        assert!((r[4] - 180.).abs() < 1e-3);
        assert!(rows(0, 240., 0.1, 0.9).is_empty());
    }

    #[test]
    fn equal_distances_along_a_bent_line_are_equal_steps_on_screen() {
        let track = Conduit::between((0., 0.), (300., 160.)).track();
        assert_eq!(track.at_distance(0.), (0., 0.));
        assert!(dist(track.at_distance(track.length()), (300., 160.)) < 0.5);
        // the raw curve parameter bunches points in the bend; 2 px steps by length must not
        let steps: Vec<f32> = (0..track.length() as i32 / 2)
            .map(|i| dist(track.at_distance(i as f32 * 2.), track.at_distance(i as f32 * 2. + 2.)))
            .collect();
        let (lo, hi) = steps
            .iter()
            .fold((f32::MAX, 0f32), |(lo, hi), s| (lo.min(*s), hi.max(*s)));
        assert!(hi - lo < 0.05, "steps from {lo} to {hi}");
    }

    #[test]
    fn every_line_moves_within_the_calm_speed_range_however_many_watts() {
        for w in [0., 1., 25., 450., 1e6] {
            let s = Drift::new(0., level(w)).speed(10.);
            assert!((SPEED.0..=SPEED.1).contains(&s), "{w} W gave {s} px/s");
        }
        assert!(level(300.) > level(30.), "a heavier flow still reads as more");
    }

    #[test]
    fn a_new_reading_changes_speed_smoothly_without_moving_the_dots() {
        let mut d = Drift::new(0., level(5.));
        let before = d.travel(3.);
        let speed_before = d.speed(3.);
        d.retarget(3., level(400.));
        assert!(
            (d.travel(3.) - before).abs() < 1e-9,
            "no jump in position at the update"
        );
        assert!(
            (d.speed(3.) - speed_before).abs() < 1e-9,
            "no jump in speed at the update"
        );
        assert!(
            (d.level(3. + EASE_S) - level(400.)).abs() < 1e-9,
            "settled after the ease"
        );
        // travel is the integral of speed through and after the ease
        let h = 1e-4;
        for t in [3.05, 3.2, 3.39, 3.6, 5.] {
            let numeric = (d.travel(t + h) - d.travel(t - h)) / (2. * h);
            assert!(
                (numeric - d.speed(t)).abs() < 1e-3,
                "at {t}: {numeric} vs {}",
                d.speed(t)
            );
        }
    }

    #[test]
    fn lit_slots_match_the_density_and_stay_evenly_spread() {
        let shown: Vec<i64> = (0..1000).filter(|&n| lit(n, 0.3, 0.25) > 0.5).collect();
        let share = shown.len() as f64 / 1000.;
        assert!((share - 0.21).abs() < 0.03, "{share}");
        let widest = shown.windows(2).map(|w| w[1] - w[0]).max().unwrap();
        assert!(
            widest as f64 <= 2. / share,
            "no gap past twice the average: {widest} slots"
        );
    }

    #[test]
    fn the_clock_stands_still_across_a_pause() {
        let mut m = Motion::default();
        let t0 = Instant::now();
        m.advance(t0, true);
        m.advance(t0 + std::time::Duration::from_millis(16), true);
        assert!((m.clock - 0.016).abs() < 1e-6);
        m.advance(t0 + std::time::Duration::from_secs(30), true);
        assert!(m.clock <= 0.016 + MAX_FRAME_S, "a long gap does not fling the dots on");
    }
}
