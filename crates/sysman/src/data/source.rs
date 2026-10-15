use std::collections::BTreeMap;

use anyhow::Result;

use crate::data::history::{Bucket, History};
use crate::data::live::Live;
use crate::data::pressure;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combine {
    Sum,
    Max,
    /// `[some.avg10, swap_in_pages_ps]` to the memory pressure index.
    Pressure,
}

impl Combine {
    fn apply(self, values: &[Option<f64>]) -> Option<f64> {
        let present = || values.iter().flatten().copied();
        match self {
            Self::Sum => present().reduce(|a, b| a + b),
            Self::Max => present().reduce(f64::max),
            Self::Pressure => {
                let swap_in = values.get(1).copied().flatten().unwrap_or(0.);
                values
                    .first()
                    .copied()
                    .flatten()
                    .map(|avg10| pressure::index(avg10, swap_in * pressure::PAGE_BYTES))
            }
        }
    }
}

/// One plotted line: the series it reads and how their simultaneous values merge into one.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub paths: Vec<String>,
    pub combine: Combine,
}

impl Source {
    pub fn new(paths: Vec<String>, combine: Combine) -> Self {
        Self { paths, combine }
    }

    pub fn query(&self, history: &History, from: i64, to: i64, buckets: u32) -> Result<Vec<Bucket>> {
        let mut rows: BTreeMap<i64, Vec<Option<Bucket>>> = BTreeMap::new();
        for (i, path) in self.paths.iter().enumerate() {
            for b in history.series(path, from, to, buckets)? {
                rows.entry(b.t).or_insert_with(|| vec![None; self.paths.len()])[i] = Some(b);
            }
        }
        Ok(rows.into_iter().filter_map(|(t, row)| self.merge(t, &row)).collect())
    }

    fn merge(&self, t: i64, row: &[Option<Bucket>]) -> Option<Bucket> {
        let field = |f: fn(&Bucket) -> f64| {
            let values: Vec<_> = row.iter().map(|b| b.as_ref().map(f)).collect();
            self.combine.apply(&values)
        };
        Some(Bucket {
            t,
            min: field(|b| b.min)?,
            avg: field(|b| b.avg)?,
            max: field(|b| b.max)?,
        })
    }

    /// Stored history up to where the live rings begin, then the rings bucketed on the same grid, so a line reaches now.
    pub fn top_up(&self, stored: &[Bucket], live: &Live, from: i64, width: i64) -> Vec<Bucket> {
        let mut rows: BTreeMap<i64, Vec<Option<f64>>> = BTreeMap::new();
        let mut start = f64::INFINITY;
        for (i, path) in self.paths.iter().enumerate() {
            let Some(ring) = live.series(path) else {
                continue;
            };
            start = start.min(ring.front().map_or(f64::INFINITY, |p| p.0));
            for &(t, v) in ring {
                let ms = (t * 1000.).round() as i64;
                rows.entry(ms).or_insert_with(|| vec![None; self.paths.len()])[i] = Some(v);
            }
        }
        if !start.is_finite() {
            return stored.to_vec();
        }
        let w = width.max(1);
        // The first bucket the ring covers whole; history keeps everything before it.
        let cut = ((start.ceil() as i64) + w - 1).div_euclid(w) * w;
        let mut out: Vec<Bucket> = stored.iter().copied().filter(|b| b.t < cut).collect();
        let mut current: Option<(Bucket, u32)> = None;
        for (ms, row) in rows {
            let Some(v) = self.combine.apply(&row) else {
                continue;
            };
            let b = ms.div_euclid(1000).div_euclid(w) * w;
            if b < cut || b < from {
                continue;
            }
            match &mut current {
                Some((bucket, n)) if bucket.t == b => {
                    bucket.min = bucket.min.min(v);
                    bucket.max = bucket.max.max(v);
                    bucket.avg += v;
                    *n += 1;
                }
                _ => {
                    if let Some((done, n)) = current.take() {
                        out.push(average(done, n));
                    }
                    current = Some((
                        Bucket {
                            t: b,
                            min: v,
                            avg: v,
                            max: v,
                        },
                        1,
                    ));
                }
            }
        }
        if let Some((done, n)) = current {
            out.push(average(done, n));
        }
        out
    }
}

fn average(mut bucket: Bucket, n: u32) -> Bucket {
    bucket.avg /= f64::from(n);
    bucket
}

/// Paths `prefix<key>suffix` whose key passes `keep`, sorted and unique.
pub fn matching<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    prefix: &str,
    suffix: &str,
    keep: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut out: Vec<String> = paths
        .into_iter()
        .filter(|p| {
            p.strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(suffix))
                .is_some_and(|key| !key.contains('/') && keep(key))
        })
        .map(str::to_owned)
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use schema::Record;
    use serde_json::json;

    use super::*;
    use crate::data::live::Event;

    fn sample(t: f64, points: &[(&str, f64)]) -> Event {
        let record: Record =
            serde_json::from_value(json!({"type": "sample", "schema": 3, "t": t, "time": "x"})).unwrap();
        Event::Record {
            record,
            points: points.iter().map(|&(p, v)| (p.to_owned(), v)).collect(),
        }
    }

    fn bucket(t: i64, v: f64) -> Bucket {
        Bucket {
            t,
            min: v,
            avg: v,
            max: v,
        }
    }

    #[test]
    fn a_sum_skips_a_missing_interface_but_never_invents_a_bucket() {
        assert_eq!(Combine::Sum.apply(&[Some(2.), None, Some(3.)]), Some(5.));
        assert_eq!(Combine::Sum.apply(&[None, None]), None, "no data stays a gap, not zero");
        assert_eq!(Combine::Max.apply(&[Some(61.), Some(74.)]), Some(74.));
        assert_eq!(Combine::Pressure.apply(&[Some(5.), None]), Some(34.));
        assert_eq!(Combine::Pressure.apply(&[None, Some(1e6)]), None);
    }

    #[test]
    fn live_rings_replace_history_from_their_first_whole_bucket() {
        let source = Source::new(vec!["/a".into(), "/b".into()], Combine::Sum);
        let mut live = Live::default();
        for i in 0..17 {
            let t = 37. + f64::from(i) * 2.;
            live.apply(sample(t, &[("/a", 1.), ("/b", f64::from(i))]));
        }
        let stored = [bucket(0, 9.), bucket(15, 9.), bucket(30, 9.), bucket(45, 9.)];
        let merged = source.top_up(&stored, &live, 0, 15);
        assert_eq!(merged.iter().map(|b| b.t).collect::<Vec<_>>(), [0, 15, 30, 45, 60]);
        assert_eq!(
            merged[2].avg, 9.,
            "the ring starts mid-bucket at 37 s, so 30 s stays historical"
        );
        // 45 s holds samples at 45..=59 s: i = 4..=11, summed with /a's 1.
        assert_eq!((merged[3].min, merged[3].max), (5., 12.));
        assert!((merged[3].avg - 8.5).abs() < 1e-9);
    }

    #[test]
    fn without_live_data_history_is_returned_whole() {
        let source = Source::new(vec!["/a".into()], Combine::Sum);
        let stored = [bucket(0, 1.), bucket(15, 2.)];
        assert_eq!(source.top_up(&stored, &Live::default(), 0, 15), stored);
    }

    #[test]
    fn matching_takes_one_key_level_and_filters_it() {
        let paths = [
            "/net/interfaces/lo/rx_bps",
            "/net/interfaces/eth0/rx_bps",
            "/net/interfaces/eth0/tx_bps",
            "/net/interfaces/wlan0/rx_bps",
            "/net/interfaces/eth0/rx_bps",
        ];
        assert_eq!(
            matching(paths, "/net/interfaces/", "/rx_bps", |k| k != "lo"),
            ["/net/interfaces/eth0/rx_bps", "/net/interfaces/wlan0/rx_bps"]
        );
    }
}
