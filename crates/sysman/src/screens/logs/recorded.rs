use std::rc::Rc;

use gpui::{
    AnyElement, Context, Div, ElementId, Hsla, IntoElement, ParentElement, Styled, div, prelude::FluentBuilder, px,
    relative,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{Icon, Sizable};
use gpui_kit_assets::IconName;

use super::inspector::heading;
use super::{Logs, model};
use crate::charts::format::{bytes, temperature};
use crate::charts::sparkline::Sparkline;
use crate::data::history::{Bucket, History, ProcField, bucket_width, db_path};
use crate::ui::shell::{Focus, Tab, navigate};
use crate::ui::theme::{Theme, rem_px, theme};

/// How far either side of an entry the inspector's charts reach.
pub(super) const AROUND_S: i64 = 600;
const POINTS: u32 = 60;

/// A span of history to read, with the moments to draw as rules; values are read at the last of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Ask {
    pub span: (i64, i64),
    pub marks: Vec<i64>,
    /// A process to chart as well, found by pid among those recorded alive at the last mark.
    pub pid: Option<u32>,
    pub points: u32,
}

impl Ask {
    pub fn around(t: i64, pid: Option<u32>) -> Ask {
        Ask {
            span: (t - AROUND_S, t + AROUND_S),
            marks: vec![t],
            pid,
            points: POINTS,
        }
    }

    /// From a little before `first` to a little after `last`, so the build-up and the aftermath both show.
    pub fn over(first: i64, last: i64, pid: Option<u32>) -> Ask {
        let pad = ((last - first) / 5).max(AROUND_S);
        let mut marks = vec![first];
        if last > first {
            marks.push(last);
        }
        Ask {
            span: (first - pad, last + pad),
            marks,
            pid,
            points: POINTS,
        }
    }

    fn at(&self) -> i64 {
        self.marks.last().copied().unwrap_or(self.span.1)
    }
}

/// What history held, as read on the background executor.
#[derive(Default)]
pub(super) struct Read {
    cpu: Vec<Bucket>,
    mem: Vec<Bucket>,
    temp: Vec<Bucket>,
    process: Option<(String, Vec<Bucket>)>,
}

pub(super) fn read(ask: &Ask) -> Read {
    let Ok(history) = History::open(&db_path()) else {
        return Read::default();
    };
    let (from, to) = ask.span;
    let series = |path: &str| history.series(path, from, to, ask.points).unwrap_or_default();
    let process = ask.pid.and_then(|pid| {
        let at = ask.at();
        let procs = history.procs_between(at - 2 * AROUND_S, at + 5).ok()?;
        let p = procs
            .into_iter()
            .filter(|p| p.pid == i64::from(pid) && p.first_t <= at)
            .max_by_key(|p| p.first_t)?;
        let rss = history.proc_series(p.id, ProcField::Rss, from, to, ask.points).ok()?;
        (!rss.is_empty()).then(|| (format!("{} memory", p.comm), rss))
    });
    Read {
        cpu: series("/cpu/total_pct"),
        mem: series("/memory/used_pct"),
        temp: series("/cpu/temp_c"),
        process,
    }
}

/// History for one [`Ask`], kept until a different one is asked for; `None` data while the read runs.
pub(super) struct Recorded {
    pub ask: Ask,
    data: Option<Data>,
}

pub(super) struct Data {
    pub width_s: f64,
    pub cpu: Rc<[Bucket]>,
    pub mem: Rc<[Bucket]>,
    pub temp: Rc<[Bucket]>,
    pub process: Option<(String, Rc<[Bucket]>)>,
}

impl Recorded {
    pub fn pending(ask: Ask) -> Recorded {
        Recorded { ask, data: None }
    }

    pub fn fill(&mut self, read: Read) {
        let fallback = bucket_width(self.ask.span.0, self.ask.span.1, self.ask.points) as f64;
        self.data = Some(Data {
            width_s: step(&read.cpu).or_else(|| step(&read.temp)).unwrap_or(fallback),
            cpu: read.cpu.into(),
            mem: read.mem.into(),
            temp: read.temp.into(),
            process: read.process.map(|(l, d)| (l, d.into())),
        });
    }

    pub fn data(&self) -> Option<&Data> {
        self.data.as_ref()
    }
}

impl Data {
    pub fn is_empty(&self) -> bool {
        self.cpu.is_empty() && self.mem.is_empty() && self.temp.is_empty()
    }
}

/// The bucket width history used, which can be coarser than asked for once raw points are pruned.
fn step(data: &[Bucket]) -> Option<f64> {
    data.windows(2)
        .map(|w| w[1].t - w[0].t)
        .filter(|d| *d > 0)
        .min()
        .map(|d| d as f64)
}

/// The reading in force at `t`: the bucket holding it, else the nearest within two buckets.
fn value_at(data: &[Bucket], t: i64, width_s: f64) -> Option<f64> {
    let w = width_s as i64;
    data.iter()
        .filter(|b| (b.t - t).abs() <= 2 * w || (b.t <= t && t < b.t + w))
        .min_by_key(|b| if b.t <= t && t < b.t + w { 0 } else { (b.t - t).abs() })
        .map(|b| b.avg)
}

/// The last reading at or before `t`, for a process that ended there.
fn last_at(data: &[Bucket], t: i64) -> Option<f64> {
    data.iter().rev().find(|b| b.t <= t).map(|b| b.avg)
}

/// One labelled strip with its value at the moment and the moments as rules.
#[allow(clippy::too_many_arguments)]
fn chart(
    id: ElementId,
    label: String,
    value: Option<String>,
    data: Rc<[Bucket]>,
    colour: Hsla,
    recorded: &Recorded,
    width_s: f64,
    height: f32,
    t: &Theme,
) -> Div {
    let (from, to) = recorded.ask.span;
    let empty = data.is_empty();
    let rules = recorded.ask.marks.iter().map(|&m| {
        let f = ((m - from) as f32 / (to - from).max(1) as f32).clamp(0., 1.);
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left(relative(f))
            .w(px(1.))
            .bg(t.text_secondary.opacity(0.55))
    });
    div()
        .flex()
        .flex_col()
        .gap(rem_px(3.))
        .min_w_0()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(rem_px(5.))
                .text_size(t.small_caption)
                .child(div().flex_shrink_0().size(rem_px(6.)).rounded_full().bg(colour))
                .child(div().min_w_0().truncate().text_color(t.text_secondary).child(label))
                .child(div().flex_1())
                .child(
                    div()
                        .flex_shrink_0()
                        .font_weight(t.weight_semibold)
                        .font_features(t.numbers.clone())
                        .text_color(if value.is_some() { t.text } else { t.text_tertiary })
                        .child(value.unwrap_or_else(|| "–".into())),
                ),
        )
        .child(
            div()
                .relative()
                .h(px(height))
                .when(empty, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom(px(3.))
                            .h(px(1.))
                            .bg(t.hairline),
                    )
                })
                .when(!empty, |d| {
                    d.child(
                        Sparkline::new(id, data, colour, (from as f64, to as f64), width_s, width_s * 3.)
                            .compact(px(height))
                            .fit(),
                    )
                })
                // the sparkline leaves 4 px at its right edge, so the rules share its scale
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left_0()
                        .right(px(4.))
                        .children(rules),
                ),
        )
}

/// CPU, memory and CPU temperature (and the process's own memory, if any) in `cols` columns.
pub(super) fn charts(id: &'static str, recorded: &Recorded, cols: u16, height: f32, t: &Theme) -> AnyElement {
    let Some(data) = recorded.data() else {
        return div().h(px(height + 18.)).into_any_element();
    };
    if data.is_empty() {
        return div()
            .text_size(t.caption)
            .text_color(t.text_tertiary)
            .child("Not recorded: sysman has no history from then")
            .into_any_element();
    }
    let at = recorded.ask.at();
    let w = data.width_s;
    let pct = |v: f64| format!("{v:.0}%");
    let mut items = vec![
        chart(
            (id, 0usize).into(),
            "CPU".into(),
            value_at(&data.cpu, at, w).map(pct),
            data.cpu.clone(),
            t.blue,
            recorded,
            w,
            height,
            t,
        ),
        chart(
            (id, 1usize).into(),
            "Memory".into(),
            value_at(&data.mem, at, w).map(pct),
            data.mem.clone(),
            t.teal,
            recorded,
            w,
            height,
            t,
        ),
        chart(
            (id, 2usize).into(),
            "CPU temperature".into(),
            value_at(&data.temp, at, w).map(|v| temperature(v, 0)),
            data.temp.clone(),
            t.orange,
            recorded,
            w,
            height,
            t,
        ),
    ];
    if let Some((label, rss)) = &data.process {
        items.push(chart(
            (id, 3usize).into(),
            label.clone(),
            last_at(rss, at).map(bytes),
            rss.clone(),
            t.purple,
            recorded,
            w,
            height,
            t,
        ));
    }
    div()
        .grid()
        .grid_cols(cols)
        .gap_x(rem_px(14.))
        .gap_y(rem_px(10.))
        .children(items)
        .into_any_element()
}

/// "Open in Explorer at 20:31", which opens Explorer's window around that moment.
pub(super) fn open_in_explorer(id: &'static str, t_s: i64, logs: &Logs) -> Button {
    let label = format!(
        "Open in Explorer at {}",
        model::clock(t_s.max(0) as u64 * 1_000_000, "%H:%M", &logs.tz)
    );
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(Icon::new(IconName::Activity))
        .label(label)
        .on_click(move |_, _, cx| navigate(Tab::Explorer, Some(Focus::Time { t: t_s }), cx))
}

impl Logs {
    /// Starts reading `ask` unless the slot already holds it; a read that lands after the slot moved on is dropped.
    pub(super) fn record(&mut self, ask: Ask, slot: fn(&mut Logs) -> &mut Option<Recorded>, cx: &mut Context<Self>) {
        if slot(self).as_ref().is_some_and(|r| r.ask == ask) {
            return;
        }
        *slot(self) = Some(Recorded::pending(ask.clone()));
        let asked = ask.clone();
        let task = cx.background_executor().spawn(async move { read(&asked) });
        cx.spawn(async move |this, cx| {
            let read = task.await;
            this.update(cx, |this, cx| {
                if let Some(r) = slot(this).as_mut().filter(|r| r.ask == ask) {
                    r.fill(read);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// The inspector's section: the charts, or why there are none, with the link to Explorer.
    pub(super) fn recorded_section(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let t = theme(cx);
        let recorded = self.entry_rec.as_ref()?;
        let at = recorded.ask.at();
        Some(
            div()
                .flex()
                .flex_col()
                .gap(rem_px(8.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_center()
                        .gap_y(rem_px(4.))
                        .child(heading("What sysman recorded", t))
                        .child(div().flex_1().min_w(rem_px(8.)))
                        .child(open_in_explorer("logs-entry-explorer", at, self)),
                )
                .child(charts("logs-entry-chart", recorded, 2, 26., t))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(t: i64, avg: f64) -> Bucket {
        Bucket {
            t,
            min: avg,
            avg,
            max: avg,
        }
    }

    #[test]
    fn the_value_shown_is_the_reading_in_force_at_the_moment() {
        let data = [b(0, 10.), b(60, 20.), b(120, 30.)];
        assert_eq!(value_at(&data, 90, 60.), Some(20.));
        assert_eq!(value_at(&data, 179, 60.), Some(30.));
        // a gap in history is not filled from a distant reading
        assert_eq!(value_at(&data, 1000, 60.), None);
    }

    #[test]
    fn a_finding_is_charted_with_room_either_side_of_it() {
        let a = Ask::over(10_000, 10_000, None);
        assert_eq!(a.span, (10_000 - AROUND_S, 10_000 + AROUND_S));
        assert_eq!(a.marks, vec![10_000]);
        let long = Ask::over(0, 36_000, None);
        assert_eq!(long.span, (-7_200, 43_200));
        assert_eq!(long.at(), 36_000);
    }
}
