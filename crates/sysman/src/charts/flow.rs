use std::cell::Cell;
use std::time::{Duration, Instant};

use gpui::{
    App, BorderStyle, Bounds, Font, Hsla, IntoElement, ParentElement, PathBuilder, Pixels, Point, RenderOnce,
    SharedString, Styled, TextAlign, TransformationMatrix, Window, canvas, div, fill, outline, point, px, size,
};
use gpui_kit_assets::IconName;

use crate::charts::trend::{dot, shape};
use crate::ui::shell::paused;
use crate::ui::theme::theme;

pub const HEIGHT: Pixels = px(240.);
/// Each step redraws the whole window, so the dots move at this rate rather than the display's.
const STEP: Duration = Duration::from_millis(42);

thread_local! {
    static STEP_DUE: Cell<bool> = const { Cell::new(false) };
}
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
    /// Sets the travelling dot's speed and size; `None` draws the line without a dot.
    pub watts: Option<f64>,
}

/// Lines from sources through hubs to sinks, each with one dot travelling at a speed set by its watts.
#[derive(IntoElement)]
pub struct EnergyFlow {
    pub sources: Vec<Source>,
    pub hubs: Vec<Hub>,
    pub sinks: Vec<Sink>,
    pub links: Vec<Link>,
    pub started: Instant,
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

/// Pixels per second; a floor keeps an idle line visibly alive.
pub fn speed(watts: f64) -> f64 {
    (12. * watts).max(16.)
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

    fn samples(&self) -> Vec<(f32, f32)> {
        (0..=24).map(|i| self.at(i as f32 / 24.)).collect()
    }

    pub fn length(&self) -> f32 {
        self.samples().windows(2).map(|w| dist(w[0], w[1])).sum()
    }

    /// The point a fraction `f` of the way along the curve by length, so a dot moves at constant speed.
    pub fn along(&self, f: f32) -> (f32, f32) {
        let pts = self.samples();
        let total = self.length();
        let mut left = f.clamp(0., 1.) * total;
        for w in pts.windows(2) {
            let d = dist(w[0], w[1]);
            if left <= d && d > 0. {
                let k = left / d;
                return (w[0].0 + (w[1].0 - w[0].0) * k, w[0].1 + (w[1].1 - w[0].1) * k);
            }
            left -= d;
        }
        self.b
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
    fn paint(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
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
        let elapsed = self.started.elapsed().as_secs_f64();

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

        for (c, link) in &conduits {
            let Some(watts) = link.watts else {
                continue;
            };
            let length = f64::from(c.length()).max(1.);
            let period = (length / speed(watts)).max(0.4);
            let f = if self.animate { (elapsed / period).fract() } else { 0.5 };
            let r = 2.85 + (watts / 40.).clamp(0., 1.) * 1.95;
            window.paint_quad(dot(p(c.along(f as f32)), px(r as f32), link.color.opacity(0.95)));
        }
        self.next_step(window, cx);
    }

    /// Asks for the next step only from a painted diagram, so frames stop when its tab closes or the window hides,
    /// and at most one step is due however often the screen redraws for its own reasons.
    fn next_step(&self, window: &Window, cx: &mut App) {
        if !self.animate || self.links.iter().all(|l| l.watts.is_none()) || paused(cx) || STEP_DUE.replace(true) {
            return;
        }
        let view = window.current_view();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(STEP).await;
            STEP_DUE.set(false);
            cx.update(|cx| cx.notify(view));
        })
        .detach();
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
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div().w_full().h(HEIGHT).child(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, cx| self.paint(bounds, window, cx),
            )
            .size_full(),
        )
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
    fn a_dot_moves_at_constant_speed_along_a_bent_line() {
        let c = Conduit::between((0., 0.), (200., 100.));
        let (start, end) = (c.along(0.), c.along(1.));
        assert_eq!(start, (0., 0.));
        assert!(dist(end, (200., 100.)) < 0.5);
        // equal fractions cover equal distances, unlike the raw curve parameter
        let quarter = dist(c.along(0.), c.along(0.25));
        let next = dist(c.along(0.25), c.along(0.5));
        assert!((quarter - next).abs() < 2., "{quarter} vs {next}");
    }

    #[test]
    fn more_watts_travel_faster_but_an_idle_line_still_moves() {
        assert_eq!(speed(0.), 16.);
        assert!(speed(30.) > speed(10.));
    }
}
