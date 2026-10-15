use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FontWeight, Hsla, IntoElement, ParentElement, Render, SharedString,
    Styled, WeakEntity, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{Icon, Sizable, WindowExt};
use gpui_kit_assets::IconName;

use crate::charts::format::Unit;
use crate::charts::trend::{ChartState, TrendChart, YScale};
use crate::data::history::{Bucket, History, bucket_width, db_path};
use crate::data::live::Live;
use crate::data::source::Source;
use crate::ui::theme::theme;
use crate::ui::widgets::legend::legend;
use crate::ui::widgets::panel::caption_label;
use crate::ui::widgets::range::{Range, range_picker};
use crate::ui::widgets::text::{caption, footnote};

const BUCKETS: u32 = 180;
const RELOAD_S: f64 = 60.;
const WIDTH: f32 = 760.;
const CHART_HEIGHT: f32 = 260.;
/// Tall enough for the chart, statistics and both explanations, so the sheet opens centred.
const EXPECTED_HEIGHT: f32 = 700.;

/// One plotted line of a detail sheet.
pub struct Series {
    pub label: SharedString,
    pub color: Hsla,
    pub source: Source,
}

/// Everything a detail sheet shows: the chart, how to scale it, and what the metric means.
pub struct Detail {
    pub title: SharedString,
    pub icon: IconName,
    pub unit: Unit,
    pub y: YScale,
    pub fill: bool,
    pub lines: Vec<Series>,
    pub rules: Vec<(f64, SharedString, Hsla)>,
    /// Explicit y ticks, such as the rule values; `None` for round ones.
    pub ticks: Option<&'static [f64]>,
    pub meaning: &'static str,
    pub method: &'static str,
    pub range: Range,
    pub live: Entity<Live>,
}

/// A click handler that builds `key`'s detail from the view at click time and opens it.
pub fn opener<V: 'static, K: 'static>(
    view: WeakEntity<V>,
    key: K,
    build: fn(&V, &K, &App) -> Option<Detail>,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    move |_, window, cx| {
        let Some(view) = view.upgrade() else {
            return;
        };
        if let Some(detail) = build(view.read(cx), &key, cx) {
            open(detail, window, cx);
        }
    }
}

pub fn open(detail: Detail, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| Sheet::new(detail, cx));
    window.open_dialog(cx, move |dialog, window, cx| {
        let t = theme(cx);
        let view = view.clone();
        let top = ((window.viewport_size().height - px(EXPECTED_HEIGHT)) / 2.).max(px(16.));
        dialog
            .w(px(WIDTH))
            .margin_top(top)
            .close_button(false)
            .p_0()
            .gap_0()
            .bg(t.window)
            .border_color(t.control_border)
            .rounded(px(12.))
            .content(move |content, _, _| content.child(view.clone()))
    });
}

/// Window statistics of one line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub average: f64,
    pub minimum: f64,
    pub maximum: f64,
}

/// Bucket means weigh equally, since every bucket spans the same time.
pub fn stats(data: &[Bucket]) -> Option<Stats> {
    if data.is_empty() {
        return None;
    }
    Some(Stats {
        average: data.iter().map(|b| b.avg).sum::<f64>() / data.len() as f64,
        minimum: data.iter().map(|b| b.min).fold(f64::INFINITY, f64::min),
        maximum: data.iter().map(|b| b.max).fold(f64::NEG_INFINITY, f64::max),
    })
}

struct Stored {
    range: Range,
    buckets: Vec<Vec<Bucket>>,
    samples: Vec<i64>,
}

fn load(sources: Vec<Source>, range: Range, to: i64) -> Result<Stored> {
    let history = History::open(&db_path())?;
    let from = to - range.span_s();
    let mut buckets = Vec::new();
    let mut samples = Vec::new();
    for source in &sources {
        buckets.push(source.query(&history, from, to, BUCKETS)?);
        // Every path of a line is sampled together, so the busiest one counts the readings.
        let mut most = 0;
        for path in &source.paths {
            most = most.max(history.sample_count(path, from, to)?);
        }
        samples.push(most);
    }
    Ok(Stored {
        range,
        buckets,
        samples,
    })
}

struct Sheet {
    detail: Detail,
    range: Range,
    stored: Option<Stored>,
    loading: bool,
    attempt: Option<(Range, f64)>,
    error: Option<SharedString>,
    merged: Vec<Rc<[Bucket]>>,
    latest: Vec<Option<f64>>,
    span: (f64, f64),
    width_s: i64,
    version: u64,
    chart: Rc<RefCell<ChartState>>,
}

impl Sheet {
    fn new(detail: Detail, cx: &mut Context<Self>) -> Self {
        cx.observe(&detail.live, |this, _, cx| this.refresh(cx)).detach();
        let mut this = Self {
            range: detail.range,
            detail,
            stored: None,
            loading: false,
            attempt: None,
            error: None,
            merged: Vec::new(),
            latest: Vec::new(),
            span: (0., 0.),
            width_s: 1,
            version: 0,
            chart: ChartState::new(),
        };
        this.refresh(cx);
        this
    }

    fn now(&self, cx: &App) -> f64 {
        self.detail.live.read(cx).latest.as_ref().map_or(0., |s| s.t)
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let now = self.now(cx);
        let stale = self.attempt.is_none_or(|(range, at)| {
            range != self.range
                || self
                    .detail
                    .live
                    .read(cx)
                    .reload_due(at, now, self.range.span_s() as f64, RELOAD_S)
        });
        if stale && !self.loading && now > 0. {
            self.loading = true;
            self.attempt = Some((self.range, now));
            let (sources, range) = (self.sources(), self.range);
            let task = cx
                .background_executor()
                .spawn(async move { load(sources, range, now as i64) });
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |this, cx| {
                    this.loading = false;
                    match result {
                        Ok(stored) => {
                            this.error = None;
                            this.stored = Some(stored);
                        }
                        Err(e) => this.error = Some(format!("History unavailable: {e}").into()),
                    }
                    this.refresh(cx);
                })
                .ok();
            })
            .detach();
        }
        self.merge(now, cx);
        cx.notify();
    }

    fn sources(&self) -> Vec<Source> {
        self.detail.lines.iter().map(|l| l.source.clone()).collect()
    }

    fn merge(&mut self, now: f64, cx: &App) {
        let live = self.detail.live.read(cx);
        let to = now as i64;
        let from = to - self.range.span_s();
        self.width_s = bucket_width(from, to, BUCKETS);
        let stored = self.stored.as_ref().filter(|s| s.range == self.range);
        self.merged.clear();
        self.latest.clear();
        for (i, line) in self.detail.lines.iter().enumerate() {
            let history = stored.and_then(|s| s.buckets.get(i)).map_or(&[][..], Vec::as_slice);
            self.merged
                .push(line.source.top_up(history, live, from, self.width_s).into());
            let newest = line.source.top_up(&[], live, to - 60, 1);
            self.latest
                .push(newest.last().map(|b| b.avg).or_else(|| history.last().map(|b| b.avg)));
        }
        self.span = (from as f64, now);
        self.version += 1;
    }

    fn set_range(&mut self, range: Range, cx: &mut Context<Self>) {
        if range != self.range {
            self.range = range;
            self.refresh(cx);
        }
    }

    /// Missing buckets wider than this are gaps, as on the screen the sheet was opened from.
    fn gap_s(&self, cx: &App) -> f64 {
        let module = self
            .detail
            .lines
            .first()
            .and_then(|l| l.source.paths.first())
            .and_then(|p| p.split('/').nth(1))
            .unwrap_or_default();
        let interval = self
            .detail
            .live
            .read(cx)
            .meta
            .as_ref()
            .and_then(|m| m.modules.get(module))
            .map_or(2, |m| m.interval_s);
        (self.width_s as f64 * 1.5).max(interval as f64 * 2.5)
    }

    fn statistics(&self, cx: &App) -> impl IntoElement {
        let t = theme(cx);
        let unit = self.detail.unit;
        let stored = self.stored.as_ref().filter(|s| s.range == self.range);
        let cell = |text: SharedString, color: Hsla| {
            div()
                .w(px(96.))
                .flex_shrink_0()
                .flex()
                .justify_end()
                .text_color(color)
                .font_features(t.numbers.clone())
                .child(text)
        };
        let header = div().flex().flex_row().pb(px(4.)).child(div().flex_1()).children(
            ["Average", "Minimum", "Maximum", "Latest", "Samples"]
                .map(|h| cell(h.into(), t.text_tertiary).text_size(t.small_caption)),
        );
        let rows = self.detail.lines.iter().enumerate().map(|(i, line)| {
            let data = self.merged.get(i).cloned().unwrap_or_else(|| Rc::from(Vec::new()));
            let s = stats(&data);
            let show = |v: Option<f64>| -> SharedString { v.map_or("–".into(), |v| precise(unit, v).into()) };
            let samples = stored
                .and_then(|s| s.samples.get(i))
                .map_or("–".into(), |n| n.to_string());
            div()
                .flex()
                .flex_row()
                .items_center()
                .py(px(5.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(div().flex_shrink_0().size(px(7.)).rounded_full().bg(line.color))
                        .child(div().truncate().child(line.label.clone())),
                )
                .child(cell(show(s.map(|s| s.average)), t.text))
                .child(cell(show(s.map(|s| s.minimum)), t.text))
                .child(cell(show(s.map(|s| s.maximum)), t.text))
                .child(cell(show(self.latest.get(i).copied().flatten()), line.color))
                .child(cell(samples.into(), t.text_secondary))
        });
        div()
            .flex()
            .flex_col()
            .text_size(t.caption)
            .child(header)
            .child(div().h(t.hairline_width).bg(t.hairline))
            .children(rows)
    }
}

/// Statistics carry one more digit than chart labels, so a quiet window does not read as all zeros.
fn precise(unit: Unit, v: f64) -> String {
    match unit {
        Unit::Percent => format!("{v:.1}%"),
        Unit::Index => format!("{v:.1}"),
        _ => unit.format(v),
    }
}

impl Render for Sheet {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx);
        let this = cx.weak_entity();
        let d = &self.detail;
        let empty = self.merged.iter().all(|m| m.is_empty());
        let mut chart = TrendChart::new(
            self.chart.clone(),
            self.version,
            self.span,
            self.width_s as f64,
            self.gap_s(cx),
        )
        .unit(d.unit)
        .y(d.y)
        .height(px(CHART_HEIGHT));
        for (line, data) in d.lines.iter().zip(&self.merged) {
            chart = chart.line(data.clone(), line.color, line.label.clone());
        }
        for (value, label, color) in &d.rules {
            chart = chart.rule(*value, label.clone(), *color);
        }
        if let Some(ticks) = d.ticks {
            chart = chart.ticks(ticks);
        }
        if d.fill {
            chart = chart.fill();
        }
        let explain = |heading: &str, body: &'static str| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(caption_label(heading, cx))
                .child(footnote(body, cx))
        };
        div()
            .flex()
            .flex_col()
            .gap(px(14.))
            .p(px(20.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(Icon::new(d.icon).small().text_color(t.text_secondary))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(d.title.clone()),
                    )
                    .child(div().flex_1())
                    .child(range_picker(
                        self.range,
                        move |range, _, cx| {
                            this.update(cx, |s, cx| s.set_range(range, cx)).ok();
                        },
                        cx,
                    )),
            )
            .when_some(self.error.clone(), |d, e| d.child(caption(e, cx).text_color(t.orange)))
            .child(if empty {
                div()
                    .h(px(CHART_HEIGHT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(caption("No recorded history for this range yet.", cx))
                    .into_any_element()
            } else {
                chart.into_any_element()
            })
            .child(legend(self.width_s, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(caption_label("Window statistics", cx))
                    .child(self.statistics(cx)),
            )
            .child(explain("What this means", d.meaning))
            .child(explain("How it is calculated", d.method))
            .child(
                div().flex().flex_row().justify_end().child(
                    Button::new("detail-done")
                        .small()
                        .primary()
                        .label("Done")
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(min: f64, avg: f64, max: f64) -> Bucket {
        Bucket { t: 0, min, avg, max }
    }

    #[test]
    fn window_extremes_come_from_bucket_extremes_not_their_means() {
        let s = stats(&[bucket(1., 2., 9.), bucket(0.5, 4., 5.)]).unwrap();
        assert_eq!(
            s.minimum, 0.5,
            "a brief dip inside a bucket is still the window minimum"
        );
        assert_eq!(
            s.maximum, 9.,
            "a brief spike inside a bucket is still the window maximum"
        );
        assert_eq!(s.average, 3.);
    }

    #[test]
    fn an_empty_window_has_no_statistics_rather_than_zeros() {
        assert_eq!(stats(&[]), None);
    }

    #[test]
    fn quiet_percentages_keep_a_decimal() {
        assert_eq!(precise(Unit::Percent, 0.34), "0.3%");
        assert_eq!(precise(Unit::Bytes, 2048.), "2.0 KB");
    }
}
