use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// One stored process reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcPoint {
    pub t: i64,
    pub cpu_pct: Option<f64>,
    pub rss_bytes: Option<i64>,
}

fn proc_row(r: &rusqlite::Row) -> rusqlite::Result<Proc> {
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
}

/// Read-only view of the daemon's `history.db`; WAL lets it read while the daemon writes.
pub struct History {
    conn: Connection,
    /// Oldest `t` per table and series or process; instances live for one load, so pruning cannot outdate it.
    oldest: RefCell<HashMap<(&'static str, i64), Option<i64>>>,
    /// Whether `procs` has `ppid` and `cgroup_id`; a database from an older daemon lacks them. Checked once.
    has_origin: Cell<Option<bool>>,
}

/// Longest recorded ancestor chain, the process included.
const MAX_LINEAGE: usize = 16;

/// Where a recorded process came from: its cgroup path and the recorded ancestors, the process first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub cgroup: String,
    pub chain: Vec<(u32, String)>,
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
            oldest: RefCell::default(),
            has_origin: Cell::default(),
        })
    }

    /// For queries one screen owns.
    pub fn conn(&self) -> &Connection {
        &self.conn
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
        let tier = self.series_tier(id, from, to)?;
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

    /// The newest stored JSON snapshot of a slow module (`netinfo`, `storage`, `power`), with its time.
    pub fn snapshot(&self, module: &str) -> Result<Option<(i64, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT t, data FROM snapshots WHERE module = ?1 ORDER BY t DESC LIMIT 1",
                [module],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Per-process history: `proc_points` up to 6 h, `proc_rollup_1m` beyond or once the raw points are pruned.
    pub fn proc_series(&self, proc_id: i64, field: ProcField, from: i64, to: i64, buckets: u32) -> Result<Vec<Bucket>> {
        let tier = self.proc_tier(&[proc_id], from, to)?;
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

    /// The recorded process instance `(pid, start_time)`, if the daemon has stored it yet.
    pub fn proc_id(&self, pid: u32, start_time: u64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM procs WHERE pid = ?1 AND start_time = ?2",
                params![pid, start_time as i64],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The cgroup and recorded parent chain of the instance `proc_id`. `None` when the daemon that recorded it
    /// predates the columns or never read its cgroup. The chain stops at PID 1, at a parent that was not recorded,
    /// or at `MAX_LINEAGE`; a parent is the instance with that pid that had started by the child's start.
    pub fn origin(&self, proc_id: i64) -> Result<Option<Origin>> {
        if !self.origin_columns()? {
            return Ok(None);
        }
        let row = self
            .conn
            .query_row(
                "SELECT p.pid, p.comm, p.ppid, p.start_time, c.path FROM procs p
                 LEFT JOIN cgroups c ON c.id = p.cgroup_id WHERE p.id = ?1",
                [proc_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((pid, comm, mut ppid, start, Some(cgroup))) = row else {
            return Ok(None);
        };
        let mut chain = vec![(pid as u32, comm)];
        let mut parent = self.conn.prepare_cached(
            "SELECT pid, comm, ppid, start_time FROM procs WHERE pid = ?1 AND start_time <= ?2
             ORDER BY (last_t >= ?2) DESC, start_time DESC LIMIT 1",
        )?;
        let mut child_start = start;
        while chain.len() < MAX_LINEAGE && chain.last().is_some_and(|(p, _)| *p != 1) {
            let Some(pp) = ppid.filter(|p| *p > 0 && !chain.iter().any(|(c, _)| i64::from(*c) == *p)) else {
                break;
            };
            let found = parent
                .query_row(params![pp, child_start], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })
                .optional()?;
            let Some((pid, comm, next_ppid, next_start)) = found else {
                break;
            };
            chain.push((pid as u32, comm));
            ppid = next_ppid;
            child_start = next_start;
        }
        Ok(Some(Origin { cgroup, chain }))
    }

    fn origin_columns(&self) -> Result<bool> {
        if let Some(known) = self.has_origin.get() {
            return Ok(known);
        }
        let mut stmt = self.conn.prepare("SELECT name FROM pragma_table_info('procs')")?;
        let names: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let known = ["ppid", "cgroup_id"].iter().all(|c| names.iter().any(|n| n == c));
        self.has_origin.set(Some(known));
        Ok(known)
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

    /// Every process instance recorded at some point in `[from, to)`.
    pub fn procs_between(&self, from: i64, to: i64) -> Result<Vec<Proc>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, pid, start_time, comm, cmdline, user, first_t, last_t FROM procs
             WHERE last_t >= ?1 AND first_t < ?2",
        )?;
        let rows = stmt.query_map([from, to], |r| {
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

    /// Recorded processes whose name contains `query`, or whose pid is `query`, most recently seen first.
    pub fn search_procs(&self, query: &str, limit: u32) -> Result<Vec<Proc>> {
        let pid = query.trim().parse::<i64>().unwrap_or(-1);
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, pid, start_time, comm, cmdline, user, first_t, last_t FROM procs
             WHERE pid = ?2 OR (?2 < 0 AND instr(lower(comm), lower(?1)) > 0) ORDER BY last_t DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![query.trim(), pid, limit], proc_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Each process's newest `proc_points` row in `(t - within, t]`, busiest first.
    pub fn procs_at(&self, t: i64, within: i64, limit: u32) -> Result<Vec<(Proc, ProcPoint)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT p.id, p.pid, p.start_time, p.comm, p.cmdline, p.user, p.first_t, p.last_t,
                    x.t, x.cpu_pct, x.rss_bytes
             FROM (SELECT proc_id, max(t) AS t FROM proc_points WHERE t <= ?1 AND t > ?1 - ?2 GROUP BY proc_id) AS m
             JOIN proc_points AS x ON x.proc_id = m.proc_id AND x.t = m.t
             JOIN procs AS p ON p.id = m.proc_id
             ORDER BY x.cpu_pct IS NULL, x.cpu_pct DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![t, within, limit], |r| {
            Ok((
                proc_row(r)?,
                ProcPoint {
                    t: r.get(8)?,
                    cpu_pct: r.get(9)?,
                    rss_bytes: r.get(10)?,
                },
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The total of `field` over the instances `ids`, summed at each instant before bucketing, so
    /// a restart counts once and the band shows the swing of the total, not of one member.
    pub fn procs_sum(&self, ids: &[i64], field: ProcField, from: i64, to: i64, buckets: u32) -> Result<Vec<Bucket>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let tier = self.proc_tier(ids, from, to)?;
        let w = width(tier, from, to, buckets);
        let (raw, [_, avg, _]) = field.columns();
        let ids = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        // A minute's total is the sample-weighted sum over the minute's scans; an instance that ran for
        // part of it adds only that part.
        let (table, total) = match tier {
            Tier::Raw => ("proc_points", format!("sum({raw})")),
            _ => (
                "proc_rollup_1m",
                format!("sum({avg} * count) / max(CASE WHEN {avg} IS NOT NULL THEN count END)"),
            ),
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT t / ?3 * ?3 AS b, min(s), avg(s), max(s) FROM (
               SELECT t, {total} AS s FROM {table} WHERE proc_id IN ({ids}) AND t >= ?1 AND t < ?2 GROUP BY t)
             WHERE s IS NOT NULL GROUP BY b ORDER BY b"
        ))?;
        let rows = stmt.query_map(params![from, to, w], |r| {
            Ok(Bucket {
                t: r.get(0)?,
                min: r.get(1)?,
                avg: r.get(2)?,
                max: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Stored readings of a series in `[from, to)`, from the tier `series` reads.
    pub fn sample_count(&self, path: &str, from: i64, to: i64) -> Result<i64> {
        let id: Option<i64> = self
            .conn
            .query_row("SELECT id FROM series WHERE path = ?1", [path], |r| r.get(0))
            .optional()?;
        let Some(id) = id else {
            return Ok(0);
        };
        let table = match self.series_tier(id, from, to)? {
            Tier::Raw => "SELECT count(*) FROM points",
            Tier::Minute => "SELECT coalesce(sum(count), 0) FROM rollup_1m",
            Tier::Quarter => "SELECT coalesce(sum(count), 0) FROM rollup_15m",
        };
        Ok(self.conn.query_row(
            &format!("{table} WHERE series_id = ?1 AND t >= ?2 AND t < ?3"),
            params![id, from, to],
            |r| r.get(0),
        )?)
    }

    /// Stored readings of one process in `[from, to)`, from the tier `proc_series` reads.
    pub fn proc_sample_count(&self, proc_id: i64, from: i64, to: i64) -> Result<i64> {
        let table = match self.proc_tier(&[proc_id], from, to)? {
            Tier::Raw => "SELECT count(*) FROM proc_points",
            _ => "SELECT coalesce(sum(count), 0) FROM proc_rollup_1m",
        };
        Ok(self.conn.query_row(
            &format!("{table} WHERE proc_id = ?1 AND t >= ?2 AND t < ?3"),
            params![proc_id, from, to],
            |r| r.get(0),
        )?)
    }

    /// The span's tier, or a coarser one when `from` predates the finer tier's data and the coarser reaches further.
    fn series_tier(&self, id: i64, from: i64, to: i64) -> Result<Tier> {
        let mut tier = Tier::for_span(to - from);
        if tier == Tier::Raw && self.coarser_reaches("points", "rollup_1m", "series_id", id, from)? {
            tier = Tier::Minute;
        }
        if tier == Tier::Minute && self.coarser_reaches("rollup_1m", "rollup_15m", "series_id", id, from)? {
            tier = Tier::Quarter;
        }
        Ok(tier)
    }

    /// Processes have no quarter-hour tier; any member whose raw points are pruned moves them all to minutes.
    fn proc_tier(&self, ids: &[i64], from: i64, to: i64) -> Result<Tier> {
        if Tier::for_span(to - from) != Tier::Raw {
            return Ok(Tier::Minute);
        }
        for &id in ids {
            if self.coarser_reaches("proc_points", "proc_rollup_1m", "proc_id", id, from)? {
                return Ok(Tier::Minute);
            }
        }
        Ok(Tier::Raw)
    }

    fn coarser_reaches(&self, fine: &'static str, coarse: &'static str, key: &str, id: i64, from: i64) -> Result<bool> {
        let Some(coarse) = self.oldest(coarse, key, id)? else {
            return Ok(false);
        };
        Ok(self
            .oldest(fine, key, id)?
            .is_none_or(|fine| from < fine && coarse < fine))
    }

    fn oldest(&self, table: &'static str, key: &str, id: i64) -> Result<Option<i64>> {
        if let Some(&t) = self.oldest.borrow().get(&(table, id)) {
            return Ok(t);
        }
        let t = self
            .conn
            .query_row(&format!("SELECT min(t) FROM {table} WHERE {key} = ?1"), [id], |r| {
                r.get(0)
            })?;
        self.oldest.borrow_mut().insert((table, id), t);
        Ok(t)
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
    fn a_short_window_older_than_the_raw_points_reads_the_rollups() {
        // raw points are kept 48 h, so a 1 h window from three days ago must come from the minute rollup
        let (dir, conn) = db();
        let now = 200 * DAY;
        let old = now - 3 * DAY;
        let ancient = now - 150 * DAY;
        conn.execute_batch(&format!(
            "INSERT INTO points (series_id, t, value) VALUES (1, {raw}, 5), (1, {now}, 6);
             INSERT INTO rollup_1m (series_id, t, min, avg, max, count) VALUES (1, {old}, 1, 2, 3, 30), (1, {now}, 6, 6, 6, 30);
             INSERT INTO rollup_15m (series_id, t, min, avg, max, count) VALUES (1, {ancient}, 9, 9, 9, 450);",
            raw = now - 2 * DAY,
        ))
        .unwrap();
        let h = history(&dir);
        let b = h.series("/cpu/total_pct", old, old + HOUR, 60).unwrap();
        assert_eq!(b.iter().map(|b| (b.t, b.avg)).collect::<Vec<_>>(), [(old, 2.)]);
        assert_eq!(h.sample_count("/cpu/total_pct", old, old + HOUR).unwrap(), 30);
        let recent = h.series("/cpu/total_pct", now - HOUR, now + 1, 60).unwrap();
        assert_eq!(
            recent.last().map(|b| b.avg),
            Some(6.),
            "a window the raw points cover still reads them"
        );
        assert_eq!(
            h.series("/cpu/total_pct", ancient, ancient + HOUR, 60).unwrap()[0].avg,
            9.,
            "past the minute rollup's retention, the quarter-hour one"
        );
    }

    #[test]
    fn a_series_newer_than_its_window_keeps_the_raw_tier() {
        // the rollups lag the raw points, so a window starting before a new series must not drop to them
        let (dir, conn) = db();
        conn.execute_batch(
            "INSERT INTO points (series_id, t, value) VALUES (1, 3000, 5), (1, 3590, 6);
             INSERT INTO rollup_1m (series_id, t, min, avg, max, count) VALUES (1, 3000, 5, 5, 5, 30);",
        )
        .unwrap();
        let b = history(&dir).series("/cpu/total_pct", 0, 3600, 60).unwrap();
        assert_eq!(b.last().map(|b| (b.t, b.avg)), Some((3540, 6.)));
    }

    #[test]
    fn process_windows_older_than_the_raw_points_read_the_minute_rollup() {
        let (dir, conn) = db();
        conn.execute_batch(
            "INSERT INTO procs VALUES (1, 42, 100, 'spin', NULL, NULL, 0, 900000);
             INSERT INTO proc_points (proc_id, t, cpu_pct) VALUES (1, 800000, 10);
             INSERT INTO proc_rollup_1m (proc_id, t, cpu_avg, cpu_max, count) VALUES (1, 600000, 70, 90, 6);",
        )
        .unwrap();
        let h = history(&dir);
        let old = 600000;
        assert_eq!(
            h.proc_series(1, ProcField::Cpu, old, old + HOUR, 60).unwrap()[0].avg,
            70.
        );
        assert_eq!(h.proc_sample_count(1, old, old + HOUR).unwrap(), 6);
        assert_eq!(
            h.procs_sum(&[1], ProcField::Cpu, old, old + HOUR, 60).unwrap()[0].avg,
            70.
        );
        assert_eq!(
            h.proc_series(1, ProcField::Cpu, 800000, 800000 + HOUR, 60).unwrap()[0].avg,
            10.
        );
    }

    #[test]
    fn snapshot_is_the_newest_of_its_module() {
        let (dir, conn) = db();
        conn.execute_batch(
            "INSERT INTO snapshots VALUES ('netinfo', 60, 'old'), ('netinfo', 120, 'new'), ('power', 180, 'other');",
        )
        .unwrap();
        let h = history(&dir);
        assert_eq!(h.snapshot("netinfo").unwrap(), Some((120, "new".into())));
        assert_eq!(h.snapshot("storage").unwrap(), None);
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

        assert_eq!(h.proc_id(42, 100).unwrap(), Some(1));
        assert_eq!(h.proc_id(42, 101).unwrap(), None, "a reused pid is another process");
        let found = h.find_procs("FIRE").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].pid, found[0].cmdline.as_deref()), (42, Some("firefox --x")));
        assert_eq!(h.find_procs("").unwrap().first().map(|p| p.id), Some(2), "newest first");
    }

    #[test]
    fn group_totals_sum_instances_at_each_scan_then_bucket() {
        let (dir, conn) = db();
        // Instance 2 restarts as 3 at t=20; 4 is outside the group.
        conn.execute_batch(
            "INSERT INTO procs VALUES (1, 10, 1, 'a', NULL, NULL, 0, 30), (2, 11, 1, 'b', NULL, NULL, 0, 10),
                                      (3, 11, 2, 'b', NULL, NULL, 20, 30), (4, 12, 1, 'c', NULL, NULL, 0, 30);
             INSERT INTO proc_points (proc_id, t, rss_bytes) VALUES
               (1, 0, 100), (1, 10, 100), (1, 20, 100), (1, 30, 100),
               (2, 0, 50), (2, 10, 70), (3, 20, 60), (3, 30, 80), (4, 0, 1000);",
        )
        .unwrap();
        let h = history(&dir);
        let one = h.procs_sum(&[1, 2, 3], ProcField::Rss, 0, 40, 1).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(
            (one[0].min, one[0].max),
            (150., 180.),
            "the totals 150, 170, 160, 180: the restart is not counted twice"
        );
        assert!((one[0].avg - 165.).abs() < 1e-9);
        assert!(h.procs_sum(&[], ProcField::Rss, 0, 40, 1).unwrap().is_empty());
        let seen: Vec<i64> = h.procs_between(15, 40).unwrap().iter().map(|p| p.id).collect();
        assert_eq!(seen, [1, 3, 4], "instance 2 ended before the window");
    }

    #[test]
    fn group_minute_totals_weight_partial_instances_by_their_samples() {
        let (dir, conn) = db();
        // Minute 60: instance 1 ran all six scans at 100, instance 2 only three at 200.
        conn.execute_batch(
            "INSERT INTO proc_rollup_1m (proc_id, t, rss_avg, rss_max, cpu_avg, count) VALUES
               (1, 60, 100, 100, 10, 6), (2, 60, 200, 200, NULL, 3),
               (1, 120, 100, 100, 10, 6), (2, 120, 300, 300, 5, 6);",
        )
        .unwrap();
        let h = history(&dir);
        let b = h.procs_sum(&[1, 2], ProcField::Rss, 0, DAY, 1440).unwrap();
        assert_eq!(
            b.iter().map(|b| (b.t, b.avg)).collect::<Vec<_>>(),
            [(60, 200.), (120, 400.)],
            "half a minute of 200 adds 100, not 200"
        );
        let cpu = h.procs_sum(&[1, 2], ProcField::Cpu, 0, DAY, 1440).unwrap();
        assert_eq!(
            cpu[0].avg, 10.,
            "a minute with no CPU reading does not shrink the divisor"
        );
    }

    #[test]
    fn explorer_queries_find_by_pid_rank_by_cpu_and_count_readings() {
        let (dir, conn) = db();
        conn.execute_batch(
            "INSERT INTO procs VALUES (1, 42, 100, 'firefox', NULL, NULL, 0, 500);
             INSERT INTO procs VALUES (2, 4242, 100, 'bash', NULL, NULL, 0, 900);
             INSERT INTO proc_points (proc_id, t, cpu_pct, rss_bytes) VALUES
               (1, 0, 90, 1), (1, 10, 5, 2), (2, 10, 40, 3), (2, 20, 70, 4);
             INSERT INTO points (series_id, t, value) VALUES (1, 0, 1), (1, 2, 1), (1, 4, 1);",
        )
        .unwrap();
        let h = history(&dir);
        let by_pid: Vec<i64> = h.search_procs("42", 10).unwrap().iter().map(|p| p.id).collect();
        assert_eq!(
            by_pid,
            [1],
            "a number matches that pid exactly, not every pid containing it"
        );
        assert_eq!(h.search_procs("BASH", 10).unwrap()[0].id, 2);

        let at = h.procs_at(15, 30, 10).unwrap();
        assert_eq!(
            at.iter().map(|(p, x)| (p.id, x.t, x.cpu_pct)).collect::<Vec<_>>(),
            [(2, 10, Some(40.)), (1, 10, Some(5.))],
            "each process's newest reading at or before the time, never a later one"
        );
        assert!(
            h.procs_at(100, 30, 10).unwrap().is_empty(),
            "stale readings are not current"
        );

        assert_eq!(h.sample_count("/cpu/total_pct", 0, 4).unwrap(), 2);
        assert_eq!(h.sample_count("/nope", 0, 4).unwrap(), 0);
        assert_eq!(h.proc_sample_count(2, 0, 60).unwrap(), 2);
    }

    fn db_with_origin() -> (tempfile::TempDir, Connection) {
        let (dir, conn) = db();
        for (table, col, kind) in schema::db::ADDED_COLUMNS {
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {col} {kind}"), [])
                .unwrap();
        }
        conn.execute("INSERT INTO cgroups VALUES (1, '/system.slice/a.service')", [])
            .unwrap();
        (dir, conn)
    }

    // id, pid, start, comm, first_t, last_t, ppid, cgroup_id
    fn add_proc(conn: &Connection, row: (i64, i64, i64, &str, i64, i64, i64, Option<i64>)) {
        conn.execute(
            "INSERT INTO procs (id, pid, start_time, comm, first_t, last_t, ppid, cgroup_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7],
        )
        .unwrap();
    }

    fn pids(o: &Origin) -> Vec<(u32, &str)> {
        o.chain.iter().map(|(p, c)| (*p, c.as_str())).collect()
    }

    #[test]
    fn the_parent_is_the_instance_that_ran_at_the_childs_start_not_just_the_pid() {
        let (dir, conn) = db_with_origin();
        add_proc(&conn, (1, 100, 1000, "old-shell", 1000, 2000, 1, None));
        add_proc(&conn, (2, 100, 5000, "new-shell", 5000, 6000, 1, None));
        add_proc(&conn, (3, 200, 1500, "child-a", 1500, 1800, 100, Some(1)));
        add_proc(&conn, (4, 200, 5100, "child-b", 5100, 5200, 100, Some(1)));
        let h = history(&dir);
        assert_eq!(pids(&h.origin(3).unwrap().unwrap())[1], (100, "old-shell"));
        assert_eq!(pids(&h.origin(4).unwrap().unwrap())[1], (100, "new-shell"));
    }

    #[test]
    fn the_chain_ends_at_pid_1() {
        let (dir, conn) = db_with_origin();
        add_proc(&conn, (1, 1, 10, "systemd", 10, 9000, 0, None));
        add_proc(&conn, (2, 50, 100, "sh", 100, 200, 1, None));
        add_proc(&conn, (3, 60, 150, "job", 150, 160, 50, Some(1)));
        let o = history(&dir).origin(3).unwrap().unwrap();
        assert_eq!(o.cgroup, "/system.slice/a.service");
        assert_eq!(pids(&o), [(60, "job"), (50, "sh"), (1, "systemd")]);
    }

    #[test]
    fn the_chain_stops_at_a_parent_that_was_never_recorded() {
        let (dir, conn) = db_with_origin();
        add_proc(&conn, (1, 60, 150, "job", 150, 160, 50, Some(1)));
        assert_eq!(pids(&history(&dir).origin(1).unwrap().unwrap()), [(60, "job")]);
    }

    #[test]
    fn the_chain_walk_is_capped() {
        let (dir, conn) = db_with_origin();
        for i in 0..40 {
            add_proc(&conn, (i + 1, 100 + i, 1000 - i, "p", 0, 9999, 101 + i, Some(1)));
        }
        let o = history(&dir).origin(1).unwrap().unwrap();
        assert_eq!(o.chain.len(), MAX_LINEAGE);
    }

    #[test]
    fn a_row_without_a_recorded_cgroup_has_no_origin() {
        let (dir, conn) = db_with_origin();
        add_proc(&conn, (1, 60, 150, "job", 150, 160, 50, None));
        assert_eq!(history(&dir).origin(1).unwrap(), None);
    }

    #[test]
    fn a_database_from_an_older_daemon_reads_as_not_recorded_not_an_error() {
        let (dir, conn) = db();
        conn.execute("INSERT INTO procs VALUES (1, 42, 100, 'old', NULL, NULL, 0, 500)", [])
            .unwrap();
        let h = history(&dir);
        assert_eq!(h.origin(1).unwrap(), None);
        assert_eq!(h.origin(1).unwrap(), None, "the cached answer agrees");
    }
}
