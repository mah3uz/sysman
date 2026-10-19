use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Bounds, Context, Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement,
    PathBuilder, Pixels, StatefulInteractiveElement, Styled, Window, canvas, div, fill, point, prelude::FluentBuilder,
    px, relative, size,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;
use journal::categories::Category;

use super::inspector::heading;
use super::model::{self, thousands};
use super::sources::category_icon;
use super::toolbar::hair_b;
use super::{LANE_BUCKETS, Lane, Logs};
use crate::data::history::Bucket;
use crate::data::logs::BootEnd;
use crate::ui::theme::{rem_px, theme};

const LANE: f32 = 17.;
const LABELS: f32 = 128.;
const FLAGS: f32 = 15.;
/// Between a boot rule and its label.
const FLAG_MARGIN: f32 = 4.;
/// The nearest mark within this many pixels of the pointer is the hovered one.
const REACH: f32 = 6.;
const MARK_MIN: f32 = 3.;

/// The bucket of the mark nearest `x` (pixels from the lane's left edge) within [`REACH`] of its edge.
fn nearest(counts: &[u32], x: f32, width: f32) -> Option<usize> {
    if counts.is_empty() || width <= 0. {
        return None;
    }
    let pitch = width / counts.len() as f32;
    let half = mark_width(pitch) / 2.;
    let centre = |b: usize| (b as f32 + 0.5) * pitch;
    let span = ((REACH + half) / pitch).ceil() as isize + 1;
    let at = (x / pitch).floor() as isize;
    (at - span..=at + span)
        .filter(|&b| b >= 0 && (b as usize) < counts.len() && counts[b as usize] > 0)
        .map(|b| (b as usize, (centre(b as usize) - x).abs()))
        .filter(|&(_, d)| d <= REACH + half)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(b, _)| b)
}

fn mark_width(pitch: f32) -> f32 {
    (pitch * 0.8).max(MARK_MIN)
}

struct Overlay {
    cpu: Rc<[Bucket]>,
    temp: Rc<[Bucket]>,
    width_s: f64,
}

struct Paint {
    lanes: Arc<Vec<Lane>>,
    max: u32,
    overlay: Option<Overlay>,
    span_s: (f64, f64),
    rules: Vec<(f32, Hsla)>,
    hover: Option<(usize, usize)>,
    wash: Hsla,
    mark: Hsla,
    crash: Hsla,
    hovered: Hsla,
    cpu_fill: Hsla,
    temp_line: Hsla,
}

/// Lane washes, then CPU as a faint area and temperature as a line over the full height, then the marks and the
/// boot rules.
fn paint(bounds: Bounds<Pixels>, p: &Paint, window: &mut Window) {
    let (left, width) = (f32::from(bounds.left()), f32::from(bounds.size.width));
    let (top, height) = (f32::from(bounds.top()), f32::from(bounds.size.height));
    let n = Category::ALL.len();
    let lane_h = height / n as f32;
    for li in (0..n).step_by(2) {
        let r = Bounds::new(
            point(px(left), px(top + li as f32 * lane_h)),
            size(px(width), px(lane_h)),
        );
        window.paint_quad(fill(r, p.wash));
    }
    if let Some(Overlay { cpu, temp, width_s }) = &p.overlay {
        let (from, to) = p.span_s;
        let x = |b: &Bucket| left + width * (((b.t as f64 + width_s / 2.) - from) / (to - from).max(1.)) as f32;
        let runs = |data: &[Bucket]| {
            let mut runs: Vec<Vec<Bucket>> = Vec::new();
            let mut last = i64::MIN / 2;
            for b in data.iter().filter(|b| b.t as f64 >= from - width_s && b.t as f64 <= to) {
                if (b.t - last) as f64 > width_s * 3. {
                    runs.push(Vec::new());
                }
                last = b.t;
                if let Some(run) = runs.last_mut() {
                    run.push(*b);
                }
            }
            runs
        };
        let bottom = top + height;
        for run in runs(cpu).iter().filter(|r| r.len() > 1) {
            let y = |v: f64| top + height * (1. - (v / 100.).clamp(0., 1.) as f32);
            let mut area = PathBuilder::fill();
            area.move_to(point(px(x(&run[0])), px(bottom)));
            for b in run {
                area.line_to(point(px(x(b)), px(y(b.avg))));
            }
            area.line_to(point(px(x(&run[run.len() - 1])), px(bottom)));
            area.close();
            if let Ok(path) = area.build() {
                window.paint_path(path, p.cpu_fill);
            }
        }
        // 20–100 °C covers idle to throttling on every CPU sysman reads
        for run in runs(temp).iter().filter(|r| r.len() > 1) {
            let y = |v: f64| top + height * (1. - ((v - 20.) / 80.).clamp(0., 1.) as f32);
            let mut line = PathBuilder::stroke(px(1.25));
            line.move_to(point(px(x(&run[0])), px(y(run[0].avg))));
            for b in &run[1..] {
                line.line_to(point(px(x(b)), px(y(b.avg))));
            }
            if let Ok(path) = line.build() {
                window.paint_path(path, p.temp_line);
            }
        }
    }
    let scale = ((1 + p.max) as f32).ln().max(f32::EPSILON);
    for (li, lane) in p.lanes.iter().enumerate() {
        let pitch = width / lane.counts.len().max(1) as f32;
        let w = mark_width(pitch);
        let base = if lane.category == Category::Crashes {
            p.crash
        } else {
            p.mark
        };
        for (b, &c) in lane.counts.iter().enumerate().filter(|(_, c)| **c > 0) {
            let hovered = p.hover == Some((li, b));
            let alpha = 0.35 + 0.65 * ((1 + c) as f32).ln() / scale;
            let x = left + (b as f32 + 0.5) * pitch - w / 2.;
            let r = Bounds::new(
                point(px(x), px(top + li as f32 * lane_h + 3.)),
                size(px(w), px(lane_h - 6.)),
            );
            let colour = if hovered {
                p.hovered
            } else {
                base.opacity(base.a * alpha)
            };
            window.paint_quad(fill(r, colour).corner_radii(px(1.)));
        }
    }
    for &(f, colour) in &p.rules {
        let r = Bounds::new(point(px(left + width * f), px(top)), size(px(1.), px(height)));
        window.paint_quad(fill(r, colour));
    }
}

impl Logs {
    fn lane_at(&self, at: gpui::Point<Pixels>) -> Option<(usize, usize)> {
        let b = self.strip.get();
        let (x, y) = (f32::from(at.x - b.origin.x), f32::from(at.y - b.origin.y));
        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
        if !(0. ..h).contains(&y) || !(-REACH..w + REACH).contains(&x) {
            return None;
        }
        let li = (y / (h / Category::ALL.len() as f32)) as usize;
        let lane = self.view.lanes.get(li)?;
        nearest(&lane.counts, x, w).map(|b| (li, b))
    }

    fn bucket_span(&self, b: usize) -> (u64, u64) {
        let (from, to) = self.view.range;
        let step = (to.saturating_sub(from)) as f64 / LANE_BUCKETS as f64;
        let at = |i: usize| from + (step * i as f64) as u64;
        (at(b), at(b + 1))
    }

    /// A click on a mark: the list narrows to that bucket of that lane.
    pub(super) fn narrow_to_mark(&mut self, (li, b): (usize, usize), cx: &mut Context<Self>) {
        let Some(category) = self.view.lanes.get(li).map(|l| l.category) else {
            return;
        };
        let window = self.bucket_span(b);
        self.set_controls(
            |c| {
                c.window = Some(window);
                c.category = Some(category);
            },
            cx,
        );
    }

    /// The strip opened out: a lane per category with its marks, what sysman recorded behind them and the boots as
    /// rules.
    pub(super) fn lanes(&self, window: &mut Window, cx: &Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let (from, to) = self.view.range;
        let frac = |us: u64| {
            if to <= from {
                0.
            } else {
                (us.saturating_sub(from) as f32 / (to - from) as f32).clamp(0., 1.)
            }
        };
        let inside = |us: u64| us > from && us < to;
        let lanes = self.view.lanes.clone();
        let boots = &self.view.boots;
        let mut rules = Vec::new();
        let mut flags = Vec::new();
        for (i, boot) in boots.iter().enumerate().rev() {
            if inside(boot.first_us) {
                rules.push((frac(boot.first_us), t.green.opacity(0.7)));
                let full = model::boot_label(i, boots);
                let short = if boot.current {
                    "Current".to_string()
                } else {
                    full.trim_start_matches("Boot ").to_string()
                };
                flags.push((frac(boot.first_us), [full, short], t.green, false, u8::from(!boot.current)));
            }
            if inside(boot.last_us) && !boot.current && boot.end != BootEnd::Running {
                let lost = boot.end == BootEnd::Abrupt;
                rules.push((
                    frac(boot.last_us),
                    if lost { t.red } else { t.text_tertiary.opacity(0.6) },
                ));
                if lost {
                    let label = format!("Power lost {}", model::clock(boot.last_us, "%H:%M:%S", &self.tz));
                    flags.push((frac(boot.last_us), [label, "Power lost".into()], t.red, true, 0));
                }
            }
        }
        flags.sort_by_key(|f| f.4);
        let width = f32::from(self.strip.get().size.width);
        let mut style = window.text_style();
        style.font_family = t.font.clone();
        style.font_weight = t.weight_medium;
        style.font_features = t.numbers.clone();
        let margin = f32::from(rem_px(FLAG_MARGIN).to_pixels(window.rem_size()));
        let measured: Vec<Placed> = flags
            .iter()
            .map(|(f, texts, _, right, _)| {
                let w = texts.clone().map(|text| {
                    let run = style.to_run(text.len());
                    f32::from(window.text_system().shape_line(text.into(), t.small_caption, &[run], None).width)
                });
                Placed {
                    at: f * width,
                    widths: w.map(|w| w + margin),
                    right: *right,
                }
            })
            .collect();
        let chosen = place(&measured, margin * 1.5);
        let flags: Vec<_> = flags
            .into_iter()
            .zip(chosen)
            .filter_map(|((f, [full, short], colour, right, _), c)| {
                c.map(|c| (f, if c == 0 { full } else { short }, colour, right))
            })
            .collect();
        let data = self.lanes_rec.as_ref().and_then(|r| r.data());
        let recorded = data.filter(|d| !d.is_empty());
        let span_s = self
            .lanes_rec
            .as_ref()
            .map_or((0., 1.), |r| (r.ask.span.0 as f64, r.ask.span.1 as f64));
        let painted = Paint {
            lanes: lanes.clone(),
            max: lanes.iter().flat_map(|l| l.counts.iter().copied()).max().unwrap_or(0),
            overlay: recorded.map(|d| Overlay {
                cpu: d.cpu.clone(),
                temp: d.temp.clone(),
                width_s: d.width_s,
            }),
            span_s,
            rules,
            hover: self.lane_hover,
            wash: t.wash.opacity(0.03),
            mark: t.text_secondary,
            crash: t.red,
            hovered: t.text,
            cpu_fill: t.blue.opacity(0.16),
            temp_line: t.orange.opacity(0.7),
        };
        let cell = self.strip.clone();
        let surface = canvas(
            move |bounds, _, _| cell.set(bounds),
            move |bounds, (), window, _| paint(bounds, &painted, window),
        )
        .absolute()
        .size_full();
        let labels = Category::ALL.into_iter().enumerate().map(|(li, c)| {
            let n: u64 = lanes
                .get(li)
                .map_or(0, |l| l.counts.iter().map(|&c| u64::from(c)).sum());
            div()
                .h(rem_px(LANE))
                .flex()
                .flex_row()
                .items_center()
                .gap(rem_px(6.))
                .pr(rem_px(10.))
                .text_size(t.caption)
                .child(Icon::new(category_icon(c)).xsmall().text_color(t.text_secondary))
                .child(div().flex_1().min_w_0().truncate().child(c.label()))
                .child(
                    div()
                        .text_size(t.small_caption)
                        .text_color(t.text_tertiary)
                        .font_features(t.numbers.clone())
                        .child(thousands(n)),
                )
        });
        let flags = flags.into_iter().map(|(f, text, colour, right)| {
            div()
                .absolute()
                .top_0()
                .map(|d| {
                    if right {
                        d.right(relative(1. - f)).mr(rem_px(FLAG_MARGIN))
                    } else {
                        d.left(relative(f)).ml(rem_px(FLAG_MARGIN))
                    }
                })
                .text_size(t.small_caption)
                .font_weight(t.weight_medium)
                .text_color(colour)
                .font_features(t.numbers.clone())
                .whitespace_nowrap()
                .child(text)
        });
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
        let slice = self.slice().map(|(a, b)| {
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(relative(a))
                .w(relative((b - a).max(0.002)))
                .border_l_1()
                .border_r_1()
                .border_color(t.blue)
                .bg(t.blue.opacity(0.10))
        });
        let tip = self.lane_hover.and_then(|(li, b)| {
            let lane = self.view.lanes.get(li)?;
            let count = *lane.counts.get(b)?;
            let (a, z) = self.bucket_span(b);
            let f = (b as f32 + 0.5) / lane.counts.len() as f32;
            let text = format!(
                "{} {} · {} · {}–{}",
                thousands(u64::from(count)),
                if count == 1 { "entry" } else { "entries" },
                lane.category.label(),
                model::clock(a, "%H:%M:%S", &self.tz),
                model::clock(z, "%H:%M:%S", &self.tz),
            );
            Some(
                div()
                    .absolute()
                    .top(rem_px(if li == 0 { LANE + 3. } else { LANE * li as f32 - 21. }))
                    .map(|d| {
                        if f > 0.6 {
                            d.right(relative(1. - f)).mr(rem_px(6.))
                        } else {
                            d.left(relative(f)).ml(rem_px(6.))
                        }
                    })
                    .px(rem_px(7.))
                    .py(rem_px(2.))
                    .rounded(rem_px(5.))
                    .bg(t.tooltip)
                    .border_1()
                    .border_color(t.hairline)
                    .text_size(t.small_caption)
                    .font_features(t.numbers.clone())
                    .whitespace_nowrap()
                    .child(text),
            )
        });
        let legend = div()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(rem_px(12.))
            .text_size(t.small_caption)
            .text_color(t.text_secondary)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(rem_px(5.))
                    .child(
                        div()
                            .w(rem_px(12.))
                            .h(rem_px(8.))
                            .rounded(px(2.))
                            .bg(t.blue.opacity(0.3)),
                    )
                    .child("CPU"),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(rem_px(5.))
                    .child(div().w(rem_px(12.)).h(px(2.)).bg(t.orange))
                    .child("CPU temperature"),
            )
            .child(div().text_color(t.text_tertiary).child(match data {
                Some(d) if d.is_empty() => "not recorded then",
                _ => "recorded by sysman",
            }));
        let area = div()
            .id("logs-lanes-area")
            .relative()
            .h(rem_px(LANE * Category::ALL.len() as f32))
            .rounded(rem_px(4.))
            .overflow_hidden()
            .when(self.lane_hover.is_some(), |d| d.cursor_pointer())
            .when(self.lane_hover.is_none(), |d| d.cursor_col_resize())
            .child(surface)
            .children(slice)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| {
                    if e.click_count >= 2 {
                        this.drag = None;
                        this.set_controls(
                            |c| {
                                c.window = None;
                                c.category = None;
                            },
                            cx,
                        );
                        return;
                    }
                    let f = this.strip_fraction(e.position);
                    this.drag = Some((f, f));
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                if this.drag.is_some() {
                    if e.pressed_button != Some(MouseButton::Left) {
                        this.end_drag(cx);
                        return;
                    }
                    let f = this.strip_fraction(e.position);
                    if let Some(d) = this.drag.as_mut() {
                        d.1 = f;
                    }
                    this.lane_hover = None;
                    cx.notify();
                    return;
                }
                let hover = this.lane_at(e.position);
                if hover != this.lane_hover {
                    this.lane_hover = hover;
                    cx.notify();
                }
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !hovered && this.lane_hover.is_some() {
                    this.lane_hover = None;
                    cx.notify();
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| this.end_drag(cx)))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, cx| this.end_drag(cx)));
        hair_b(
            div()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap(rem_px(4.))
                .px(rem_px(14.))
                .pt(rem_px(6.))
                .pb(rem_px(6.)),
            t,
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(rem_px(10.))
                .child(div().w(rem_px(LABELS)).child(heading("Lanes", t)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(t.small_caption)
                        .text_color(t.text_tertiary)
                        .child("Drag across to narrow the time · click a mark for its entries · double-click to clear"),
                )
                .child(legend)
                .child(
                    Button::new("logs-lanes")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::ChevronUp))
                        .tooltip("Back to the strip")
                        .on_click(cx.listener(|this, _, _, cx| this.set_lanes_open(false, cx))),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .child(
                    div()
                        .w(rem_px(LABELS))
                        .flex_shrink_0()
                        .pt(rem_px(FLAGS))
                        .children(labels),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().relative().h(rem_px(FLAGS)).children(flags))
                        .child(div().relative().child(area).children(tip))
                        .child(div().relative().h(rem_px(11.)).mt(rem_px(2.)).children(ticks)),
                ),
        )
    }
}

/// A label above the lanes, in pixels: the rule it hangs from, its widths (full then short, margin included) and
/// whether it reads leftwards from the rule.
struct Placed {
    at: f32,
    widths: [f32; 2],
    right: bool,
}

/// Which form of each label fits (0 full, 1 short) or `None` where neither does, placing them in order so the
/// earlier ones win; `gap` is the least space kept between two labels.
fn place(labels: &[Placed], gap: f32) -> Vec<Option<usize>> {
    let mut taken: Vec<(f32, f32)> = Vec::new();
    labels
        .iter()
        .map(|l| {
            let form = (0..2).find(|&i| {
                let w = l.widths[i];
                let span = if l.right { (l.at - w, l.at) } else { (l.at, l.at + w) };
                taken.iter().all(|&(a, b)| span.1 + gap <= a || span.0 >= b + gap)
            })?;
            let w = l.widths[form];
            taken.push(if l.right { (l.at - w, l.at) } else { (l.at, l.at + w) });
            Some(form)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hovered_mark_is_the_nearest_one_within_reach() {
        // 10 buckets over 100 px: centres at 5, 15, 25…; marks 8 px wide
        let counts = [0, 3, 0, 0, 0, 0, 0, 1, 1, 0];
        assert_eq!(nearest(&counts, 15., 100.), Some(1));
        assert_eq!(nearest(&counts, 24., 100.), Some(1));
        // 10.5 px past bucket 1's edge and far from bucket 7: nothing
        assert_eq!(nearest(&counts, 29.5, 100.), None);
        assert_eq!(nearest(&counts, 81., 100.), Some(8));
    }

    #[test]
    fn crowded_boot_labels_shorten_or_drop_rather_than_overlap() {
        let label = |at: f32, right: bool| Placed {
            at,
            widths: [50., 20.],
            right,
        };
        // a power loss and the current boot first, then older boots newest first, 25 px apart
        let labels = [
            label(100., true),
            label(200., false),
            label(175., false),
            label(150., false),
            label(125., false),
            label(20., false),
        ];
        let placed = place(&labels, 6.);
        assert_eq!(placed[0], Some(0), "the power loss keeps its full label");
        assert_eq!(placed[1], Some(0), "so does the current boot");
        assert_eq!(placed[2], None, "too close to the current boot even for the short form");
        assert_eq!(placed[3], Some(1), "room for the short form only");
        assert_eq!(placed[4], None, "its short form would touch the one before it");
        assert_eq!(placed[5], Some(1), "the power loss's label reaches back to 50");
        let spans: Vec<(f32, f32)> = labels
            .iter()
            .zip(&placed)
            .filter_map(|(l, p)| {
                let w = l.widths[(*p)?];
                Some(if l.right { (l.at - w, l.at) } else { (l.at, l.at + w) })
            })
            .collect();
        for (i, a) in spans.iter().enumerate() {
            for b in &spans[i + 1..] {
                assert!(a.1 + 6. <= b.0 || b.1 + 6. <= a.0, "{a:?} and {b:?} overlap");
            }
        }
    }

    #[test]
    fn marks_stay_wide_enough_to_hit_on_a_dense_lane() {
        assert_eq!(mark_width(1.), MARK_MIN);
        assert!(mark_width(10.) > MARK_MIN);
    }
}
