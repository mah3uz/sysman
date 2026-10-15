use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

const RAW_MAX_S: i64 = 6 * 3600;
const MINUTE_MAX_S: i64 = 7 * 86400;

/// One plotted step. A bucket with no data is absent, never zero, so a chart can leave the gap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub t: i64,
    pub min: f64,
    pub avg: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Raw,
    Minute,
    Quarter,
}

impl Tier {
    pub fn for_span(span_s: i64) -> Self {
        if span_s <= RAW_MAX_S {
            Self::Raw
        } else if span_s <= MINUTE_MAX_S {
            Self::Minute
        } else {
            Self::Quarter
        }
    }

    fn resolution_s(self) -> i64 {
        match self {
            Self::Raw => 1,
            Self::Minute => 60,
            Self::Quarter => 900,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcField {
    Cpu,
    Rss,
    Threads,
    Fds,
    Read,
    Write,
}

impl ProcField {
    /// `proc_points` column, then the `proc_rollup_1m` columns for min, weighted avg and max.
    fn columns(self) -> (&'static str, [&'static str; 3]) {
        match self {
            Self::Cpu => ("cpu_pct", ["cpu_avg", "cpu_avg", "cpu_max"]),
            Self::Rss => ("rss_bytes", ["rss_avg", "rss_avg", "rss_max"]),
            Self::Threads => ("threads", ["threads_max"; 3]),
            Self::Fds => ("fds", ["fds_max"; 3]),
            Self::Read => ("read_bps", ["read_bps_avg"; 3]),
            Self::Write => ("write_bps", ["write_bps_avg"; 3]),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub id: i64,
    pub pid: i64,
    pub start_time: i64,
    pub comm: String,
    pub cmdline: Option<String>,
    pub user: Option<String>,
    pub first_t: i64,
    pub last_t: i64,
}

/// Read-only view of the daemon's `history.db`; WAL lets it read while the daemon writes.
pub struct History {
    conn: Connection,
}

pub fn db_path() -> PathBuf {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) if Path::new(&dir).is_absolute() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"),
    };
    base.join("sysman/history.db")
}

/// Bucket width in seconds: about `buckets` steps over the span, whole multiples of the tier's resolution.
fn width(tier: Tier, from: i64, to: i64, buckets: u32) -> i64 {
    let res = tier.resolution_s();
    let n = i64::from(buckets.max(1));
    let raw = ((to - from).max(1) + n - 1) / n;
    (raw + res - 1) / res * res
}

/// The step `series` will use for this span, so callers can bucket live points on the same grid.
pub fn bucket_width(from: i64, to: i64, buckets: u32) -> i64 {
    width(Tier::for_span(to - from), from, to, buckets)
}

impl History {
    pub fn open(path: &Path) -> Result<Self> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        Ok(Self {
            conn: Connection::open_with_flags(path, flags)?,
        })
    }

    /// Buckets over `[from, to)` for a series named by JSON pointer; empty when the series is unknown.
    pub fn series(&self, path: &str, from: i64, to: i64, buckets: u32) -> Result<Vec<Bucket>> {
        let id: Option<i64> = self
            .conn
            .query_row("SELECT id FROM series WHERE path = ?1", [path], |r| r.get(0))
            .optional()?;
        let Some(id) = id else {
            return Ok(Vec::new());
        };
        let tier = Tier::for_span(to - from);
        let w = width(tier, from, to, buckets);
        let (table, aggregates) = match tier {
            Tier::Raw => ("points", "min(value), avg(value), max(value)"),
            Tier::Minute => ("rollup_1m", "min(min), sum(avg * count) / sum(count), max(max)"),
            Tier::Quarter => ("rollup_15m", "min(min), sum(avg * count) / sum(count), max(max)"),
        };
        self.bucketed(table, "series_id", id, aggregates, from, to, w)
    }

    /// Every series path that starts with `prefix`.
    pub fn paths(&self, prefix: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT path FROM series WHERE substr(path, 1, length(?1)) = ?1")?;
        let rows = stmt.query_map([prefix], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Per-process history: `proc_points` up to 6 h, `proc_rollup_1m` beyond.
    pub fn proc_series(&self, proc_id: i64, field: ProcField, from: i64, to: i64, buckets: u32) -> Result<Vec<Bucket>> {
        let tier = match Tier::for_span(to - from) {
            Tier::Raw => Tier::Raw,
            Tier::Minute | Tier::Quarter => Tier::Minute,
        };
        let w = width(tier, from, to, buckets);
        let (raw, [lo, avg, hi]) = field.columns();
        let (table, aggregates) = match tier {
            Tier::Raw => ("proc_points", format!("min({raw}), avg({raw}), max({raw})")),
            _ => (
                "proc_rollup_1m",
                format!("min({lo}), sum({avg} * count) / sum(CASE WHEN {avg} IS NOT NULL THEN count END), max({hi})"),
            ),
        };
        self.bucketed(table, "proc_id", proc_id, &aggregates, from, to, w)
    }

    /// Processes whose command name contains `name`, ignoring case, most recently seen first.
    pub fn find_procs(&self, name: &str) -> Result<Vec<Proc>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, pid, start_time, comm, cmdline, user, first_t, last_t FROM procs
             WHERE instr(lower(comm), lower(?1)) > 0 ORDER BY last_t DESC LIMIT 500",
        )?;
        let rows = stmt.query_map([name], |r| {
            Ok(Proc {
                id: r.get(0)?,
                pid: r.get(1)?,
                start_time: r.get(2)?,
                comm: r.get(3)?,
                cmdline: r.get(4)?,
                user: r.get(5)?,
                first_t: r.get(6)?,
                last_t: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    #[allow(clippy::too_many_arguments)]
    fn bucketed(
        &self,
        table: &str,
        key: &str,
        id: i64,
        aggregates: &str,
        from: i64,
        to: i64,
        width: i64,
    ) -> Result<Vec<Bucket>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT t / ?4 * ?4 AS b, {aggregates} FROM {table}
             WHERE {key} = ?1 AND t >= ?2 AND t < ?3 GROUP BY b ORDER BY b"
        ))?;
        let rows = stmt.query_map(params![id, from, to, width], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<f64>>(1)?,
                r.get::<_, Option<f64>>(2)?,
                r.get::<_, Option<f64>>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            if let (t, Some(min), Some(avg), Some(max)) = row? {
                out.push(Bucket { t, min, avg, max });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3600;
    const DAY: i64 = 86400;

    fn db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("history.db")).unwrap();
        conn.execute_batch(schema::db::SCHEMA).unwrap();
        conn.execute("INSERT INTO series (id, path) VALUES (1, '/cpu/total_pct')", [])
            .unwrap();
        (dir, conn)
    }

    fn history(dir: &tempfile::TempDir) -> History {
        History::open(&dir.path().join("history.db")).unwrap()
    }

    #[test]
    fn tier_follows_the_span_at_each_boundary() {
        assert_eq!(Tier::for_span(6 * HOUR), Tier::Raw);
        assert_eq!(Tier::for_span(6 * HOUR + 1), Tier::Minute);
        assert_eq!(Tier::for_span(7 * DAY), Tier::Minute);
        assert_eq!(Tier::for_span(7 * DAY + 1), Tier::Quarter);
    }

    #[test]
    fn widths_are_whole_rollup_steps() {
        assert_eq!(width(Tier::Raw, 0, 1800, 120), 15);
        assert_eq!(width(Tier::Minute, 0, DAY, 120), 720);
        assert_eq!(width(Tier::Minute, 0, 7 * HOUR, 120), 240, "210 s rounds up to 4 min");
        assert_eq!(width(Tier::Quarter, 0, 30 * DAY, 120), 21600);
    }

    #[test]
    fn raw_points_bucket_with_a_gap_left_empty() {
        let (dir, conn) = db();
        // 0..60 s every 2 s, nothing from 60 to 120, then 120..180 s.
        for t in (0..60).step_by(2).chain((120..180).step_by(2)) {
            conn.execute(
                "INSERT INTO points (series_id, t, value) VALUES (1, ?1, ?2)",
                params![t, 10.0 + f64::from(t % 10)],
            )
            .unwrap();
        }
        let b = history(&dir).series("/cpu/total_pct", 0, 180, 6).unwrap();
        assert_eq!(
            b.iter().map(|b| b.t).collect::<Vec<_>>(),
            [0, 30, 120, 150],
            "60..120 is a gap"
        );
        assert!(b.iter().all(|b| b.min >= 10.0), "a gap must not read as zero");
        assert_eq!((b[0].min, b[0].max), (10.0, 18.0));
        assert!((b[0].avg - 14.0).abs() < 1e-9);
    }

    #[test]
    fn rollups_merge_min_max_and_weight_the_average_by_count() {
        let (dir, conn) = db();
        let base = 1000 * 60;
        // Two minutes merge into one 2-minute bucket: (avg 10 over 30 samples) and (avg 40 over 10).
        for (t, min, avg, max, count) in [(base, 5.0, 10.0, 20.0, 30), (base + 60, 1.0, 40.0, 90.0, 10)] {
            conn.execute(
                "INSERT INTO rollup_1m (series_id, t, min, avg, max, count) VALUES (1, ?1, ?2, ?3, ?4, ?5)",
                params![t, min, avg, max, count],
            )
            .unwrap();
        }
        let b = history(&dir)
            .series("/cpu/total_pct", base, base + 2 * DAY, 1440)
            .unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].t, b[0].min, b[0].max), (base, 1.0, 90.0));
        assert!((b[0].avg - 17.5).abs() < 1e-9, "count-weighted, not the mean of means");
    }

    #[test]
    fn long_ranges_read_the_quarter_hour_rollup() {
        let (dir, conn) = db();
        conn.execute(
            "INSERT INTO rollup_1m (series_id, t, min, avg, max, count) VALUES (1, 900, 1, 1, 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO rollup_15m (series_id, t, min, avg, max, count) VALUES (1, 900, 7, 7, 7, 1)",
            [],
        )
        .unwrap();
        let h = history(&dir);
        assert_eq!(h.series("/cpu/total_pct", 0, 30 * DAY, 120).unwrap()[0].avg, 7.0);
        assert_eq!(h.series("/cpu/total_pct", 0, 7 * DAY, 120).unwrap()[0].avg, 1.0);
        assert!(h.series("/nope", 0, 7 * DAY, 120).unwrap().is_empty());
    }

    #[test]
    fn process_history_and_search() {
        let (dir, conn) = db();
        conn.execute_batch(
            "INSERT INTO procs VALUES (1, 42, 100, 'Firefox', 'firefox --x', 'me', 0, 500);
             INSERT INTO procs VALUES (2, 43, 100, 'bash', NULL, NULL, 0, 900);
             INSERT INTO proc_points (proc_id, t, cpu_pct, rss_bytes) VALUES (1, 0, 10, 100), (1, 10, 30, 300), (1, 20, NULL, 200);
             INSERT INTO proc_rollup_1m (proc_id, t, cpu_avg, cpu_max, fds_max, count) VALUES
               (1, 0, 20, 50, 9, 6), (1, 60, NULL, NULL, 12, 6), (1, 120, 50, 80, 10, 2);",
        )
        .unwrap();
        let h = history(&dir);
        let cpu = h.proc_series(1, ProcField::Cpu, 0, 60, 2).unwrap();
        assert_eq!(
            cpu,
            [Bucket {
                t: 0,
                min: 10.0,
                avg: 20.0,
                max: 30.0
            }]
        );

        let cpu = h.proc_series(1, ProcField::Cpu, 0, DAY, 1).unwrap();
        assert_eq!(cpu.len(), 1);
        assert!(
            (cpu[0].avg - 27.5).abs() < 1e-9,
            "the minute with no CPU reading does not dilute the average"
        );
        assert_eq!((cpu[0].min, cpu[0].max), (20.0, 80.0));
        assert_eq!(h.proc_series(1, ProcField::Fds, 0, DAY, 1).unwrap()[0].max, 12.0);

        let found = h.find_procs("FIRE").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].pid, found[0].cmdline.as_deref()), (42, Some("firefox --x")));
        assert_eq!(h.find_procs("").unwrap().first().map(|p| p.id), Some(2), "newest first");
    }
}
