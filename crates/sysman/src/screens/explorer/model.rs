use jiff::Timestamp;

use crate::charts::format::Unit;
use crate::data::history::Bucket;

pub const SPANS: [(i64, &str); 6] = [
    (900, "15 min"),
    (3600, "1 hr"),
    (6 * 3600, "6 hr"),
    (86400, "24 hr"),
    (7 * 86400, "7 day"),
    (30 * 86400, "30 day"),
];
/// Narrower than this, a 2 s series shows too few readings to read as a line.
pub const MIN_SPAN: i64 = 120;
pub const MAX_SPAN: i64 = 30 * 86400;
pub const ZOOM_IN: f64 = 0.5;
pub const ZOOM_OUT: f64 = 2.;
pub const MAX_PROCESSES: usize = 8;

/// The visible time window: following now, or fixed at `end`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub span: i64,
    pub end: f64,
    pub live: bool,
}

impl View {
    pub fn live(span: i64) -> Self {
        Self {
            span,
            end: 0.,
            live: true,
        }
    }

    pub fn range(&self, now: f64) -> (f64, f64) {
        let end = if self.live { now } else { self.end.min(now) };
        (end - self.span as f64, end)
    }

    fn fix(&mut self, end: f64, now: f64) {
        self.end = end.min(now);
        self.live = false;
    }

    /// Earlier or later by one whole span; reaching now follows live again.
    pub fn step(&mut self, dir: i32, now: f64) {
        let end = self.range(now).1 + f64::from(dir) * self.span as f64;
        if end >= now {
            self.live = true;
        } else {
            self.fix(end, now);
        }
    }

    /// A new span ending where the window ends now.
    pub fn set_span(&mut self, span: i64, now: f64) {
        let end = self.range(now).1;
        self.span = span.clamp(MIN_SPAN, MAX_SPAN);
        if !self.live {
            self.fix(end, now);
        }
    }

    /// Scales the span keeping `anchor` at the same place on screen; with no anchor, around the centre,
    /// or around now while following live.
    pub fn zoom(&mut self, factor: f64, anchor: Option<f64>, now: f64) {
        let (from, end) = self.range(now);
        let span = ((self.span as f64 * factor).round() as i64).clamp(MIN_SPAN, MAX_SPAN);
        if self.live && anchor.is_none() {
            self.span = span;
            return;
        }
        let anchor = anchor.unwrap_or((from + end) / 2.).clamp(from, end);
        let fraction = (anchor - from) / self.span as f64;
        let new_end = anchor + (1. - fraction) * span as f64;
        self.span = span;
        if new_end >= now {
            self.end = now;
        } else {
            self.fix(new_end, now);
        }
    }

    /// Dragging: the window moves by `seconds`, later when positive.
    pub fn pan(&mut self, seconds: f64, now: f64) {
        let end = self.range(now).1 + seconds;
        self.fix(end, now);
    }

    /// Centres the window on `t`, as far as now allows.
    pub fn centre_on(&mut self, t: f64, now: f64) {
        self.fix(t + self.span as f64 / 2., now);
    }

    pub fn toggle_live(&mut self, now: f64) {
        if self.live {
            self.fix(now, now);
        } else {
            self.live = true;
        }
    }
}

/// `1 hr` for a preset span, otherwise the nearest whole unit: `30 s`, `30 min`, `3 hr`, `2 day`.
pub fn span_label(span: i64) -> String {
    if let Some((_, label)) = SPANS.iter().find(|(s, _)| *s == span) {
        return (*label).into();
    }
    if span < 60 {
        format!("{span} s")
    } else if span < 3600 {
        format!("{} min", (span as f64 / 60.).round())
    } else if span < 2 * 86400 {
        format!("{} hr", (span as f64 / 3600.).round())
    } else {
        format!("{} day", (span as f64 / 86400.).round())
    }
}

/// Columns in the chart grid for the width of the chart area.
pub fn columns(width: f32, grid: bool) -> usize {
    match (grid, width) {
        (false, _) => 1,
        (true, w) if w >= 760. => 3,
        _ => 2,
    }
}

/// The first colour slot no compared process holds.
pub fn free_slot(used: &[usize]) -> Option<usize> {
    (0..MAX_PROCESSES).find(|slot| !used.contains(slot))
}

/// Buckets that overlap `[from, to]`.
fn visible(data: &[Bucket], from: f64, to: f64, width: f64) -> impl Iterator<Item = &Bucket> {
    data.iter()
        .filter(move |b| b.t as f64 + width > from && b.t as f64 <= to)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub average: f64,
    pub minimum: f64,
    pub maximum: f64,
}

/// Summary of the visible window. Buckets are equally wide, so their means weigh the same.
pub fn stats(data: &[Bucket], from: f64, to: f64, width: f64) -> Option<Stats> {
    let mut n = 0;
    let mut sum = 0.;
    let (mut minimum, mut maximum) = (f64::INFINITY, f64::NEG_INFINITY);
    for b in visible(data, from, to, width) {
        n += 1;
        sum += b.avg;
        minimum = minimum.min(b.min);
        maximum = maximum.max(b.max);
    }
    (n > 0).then(|| Stats {
        average: sum / f64::from(n),
        minimum,
        maximum,
    })
}

/// The bucket covering `t`; a gap gives nothing rather than a neighbour's value.
pub fn at(data: &[Bucket], t: f64, width: f64) -> Option<&Bucket> {
    data.iter().find(|b| b.t as f64 <= t && t < b.t as f64 + width)
}

/// The middle of the bucket before or after the one holding `t`, kept inside `[from, to]`.
pub fn step(t: f64, dir: i32, width: f64, from: f64, to: f64) -> f64 {
    let index = (t / width).floor() + f64::from(dir);
    (index * width + width / 2.).clamp(from, to)
}

pub fn unit_name(unit: Unit) -> &'static str {
    match unit {
        Unit::Percent => "percent",
        Unit::Index => "index",
        Unit::Bytes => "bytes",
        Unit::BytesPerSec => "bytes/s",
        Unit::Celsius => "celsius",
        Unit::Millis => "ms",
        Unit::Count => "count",
        Unit::Watts => "watts",
    }
}

pub struct CsvLine<'a> {
    pub chart: &'a str,
    pub series: &'a str,
    pub unit: Unit,
    pub data: &'a [Bucket],
}

/// A spreadsheet must not run a cell as a formula, and quotes inside a cell are doubled.
fn cell(text: &str) -> String {
    let guarded = if text.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{text}")
    } else {
        text.to_string()
    };
    format!("\"{}\"", guarded.replace('"', "\"\""))
}

/// Every plotted bucket in the window, one row each, timestamps in UTC.
pub fn csv(lines: &[CsvLine], from: f64, to: f64, width: f64) -> String {
    let mut out = String::from("chart,series,unit,start,average,minimum,maximum,seconds\n");
    for line in lines {
        for b in visible(line.data, from, to, width) {
            let start = Timestamp::from_second(b.t).map(|t| t.to_string()).unwrap_or_default();
            out.push_str(&format!(
                "{},{},{},{start},{},{},{},{width}\n",
                cell(line.chart),
                cell(line.series),
                unit_name(line.unit),
                b.avg,
                b.min,
                b.max
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_000_000.;

    fn bucket(t: i64, min: f64, avg: f64, max: f64) -> Bucket {
        Bucket { t, min, avg, max }
    }

    #[test]
    fn stepping_moves_a_whole_span_and_returns_to_live_at_now() {
        let mut v = View::live(3600);
        assert_eq!(v.range(NOW), (NOW - 3600., NOW));
        v.step(-1, NOW);
        assert!(!v.live);
        assert_eq!(v.range(NOW), (NOW - 7200., NOW - 3600.));
        assert_eq!(
            v.range(NOW + 500.),
            (NOW - 7200., NOW - 3600.),
            "a fixed window does not drift"
        );
        v.step(1, NOW);
        assert!(v.live, "forward to now follows live again");
    }

    #[test]
    fn zoom_keeps_the_anchor_on_the_same_spot_and_respects_limits() {
        let mut v = View::live(3600);
        v.step(-1, NOW);
        let (from, _) = v.range(NOW);
        let anchor = from + 900.;
        v.zoom(ZOOM_IN, Some(anchor), NOW);
        let (from, to) = v.range(NOW);
        assert_eq!(to - from, 1800.);
        assert!(
            ((anchor - from) / (to - from) - 0.25).abs() < 1e-9,
            "anchor stays a quarter in"
        );

        let mut v = View::live(3600);
        v.zoom(ZOOM_IN, None, NOW);
        assert!(v.live, "zooming a live window without a cursor keeps following now");
        assert_eq!(v.span, 1800);

        v.zoom(1e-6, None, NOW);
        assert_eq!(v.span, MIN_SPAN);
        v.zoom(1e9, None, NOW);
        assert_eq!(v.span, MAX_SPAN);
    }

    #[test]
    fn zooming_out_past_now_is_clamped_to_now() {
        let mut v = View::live(3600);
        v.zoom(ZOOM_OUT, Some(NOW - 60.), NOW);
        assert_eq!(v.range(NOW), (NOW - 7200., NOW));
    }

    #[test]
    fn dragging_and_go_to_time_fix_the_window_but_never_past_now() {
        let mut v = View::live(600);
        v.pan(-300., NOW);
        assert!(!v.live);
        assert_eq!(v.range(NOW).1, NOW - 300.);
        v.pan(10_000., NOW);
        assert_eq!(v.range(NOW).1, NOW, "the future has no data");
        v.centre_on(NOW - 5000., NOW);
        assert_eq!(v.range(NOW), (NOW - 5300., NOW - 4700.));
        v.toggle_live(NOW);
        assert!(v.live);
        v.toggle_live(NOW);
        assert_eq!((v.live, v.end), (false, NOW), "pausing freezes the current window");
    }

    #[test]
    fn spans_are_labelled_like_the_picker() {
        assert_eq!(span_label(3600), "1 hr");
        assert_eq!(span_label(30 * 86400), "30 day");
        assert_eq!(span_label(1800), "30 min");
        assert_eq!(span_label(30), "30 s", "bucket widths are labelled the same way");
        assert_eq!(span_label(3 * 3600), "3 hr");
        assert_eq!(span_label(3 * 86400 + 600), "3 day");
    }

    #[test]
    fn grid_and_colour_slots() {
        assert_eq!(columns(900., true), 3);
        assert_eq!(columns(600., true), 2);
        assert_eq!(columns(900., false), 1);
        assert_eq!(
            free_slot(&[0, 1, 3]),
            Some(2),
            "a removed process frees its colour for the next"
        );
        assert_eq!(free_slot(&[0, 1, 2, 3, 4, 5, 6, 7]), None);
    }

    #[test]
    fn statistics_cover_only_the_visible_window() {
        let data = [
            bucket(0, 1., 2., 3.),
            bucket(60, 0., 10., 50.),
            bucket(120, 5., 6., 7.),
            bucket(600, 90., 90., 90.),
        ];
        let s = stats(&data, 30., 180., 60.).unwrap();
        assert_eq!(s.minimum, 0.);
        assert_eq!(s.maximum, 50.);
        assert!(
            (s.average - 6.).abs() < 1e-9,
            "(2 + 10 + 6) / 3, the bucket at 600 is outside"
        );
        assert_eq!(
            stats(&data, 200., 500., 60.),
            None,
            "an empty window has no statistics, not zeros"
        );
    }

    #[test]
    fn the_value_at_the_cursor_is_the_bucket_covering_it() {
        let data = [bucket(0, 1., 2., 3.), bucket(60, 4., 5., 6.), bucket(180, 7., 8., 9.)];
        assert_eq!(at(&data, 75., 60.).map(|b| b.avg), Some(5.));
        assert_eq!(at(&data, 150., 60.), None, "inside a gap");
        assert_eq!(step(75., 1, 60., 0., 240.), 150.);
        assert_eq!(step(75., -1, 60., 0., 240.), 30.);
        assert_eq!(step(10., -1, 60., 0., 240.), 0., "stops at the window's edge");
    }

    #[test]
    fn csv_has_one_row_per_visible_bucket_and_cannot_inject_formulas() {
        let data = [bucket(0, 1., 2., 3.), bucket(60, 4., 5., 6.), bucket(600, 0., 0., 0.)];
        let lines = [CsvLine {
            chart: "=cmd|' /C calc'!A0",
            series: "Read \"fast\"",
            unit: Unit::BytesPerSec,
            data: &data,
        }];
        let text = csv(&lines, 30., 120., 60.);
        let rows: Vec<&str> = text.lines().collect();
        assert_eq!(rows[0], "chart,series,unit,start,average,minimum,maximum,seconds");
        assert_eq!(rows.len(), 3, "header and the two buckets overlapping the window");
        assert_eq!(
            rows[1],
            "\"'=cmd|' /C calc'!A0\",\"Read \"\"fast\"\"\",bytes/s,1970-01-01T00:00:00Z,2,1,3,60"
        );
    }
}
