use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;

use anyhow::{Context, Result};
use rusqlite::{Connection, Transaction, params};
use serde_json::Value;

use crate::config::Retention;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS series (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS points (
    series_id INTEGER NOT NULL, t INTEGER NOT NULL, value REAL NOT NULL,
    PRIMARY KEY (series_id, t)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS rollup_1m (
    series_id INTEGER NOT NULL, t INTEGER NOT NULL,
    min REAL NOT NULL, avg REAL NOT NULL, max REAL NOT NULL, count INTEGER NOT NULL,
    PRIMARY KEY (series_id, t)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS rollup_15m (
    series_id INTEGER NOT NULL, t INTEGER NOT NULL,
    min REAL NOT NULL, avg REAL NOT NULL, max REAL NOT NULL, count INTEGER NOT NULL,
    PRIMARY KEY (series_id, t)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS snapshots (
    module TEXT NOT NULL, t INTEGER NOT NULL, data TEXT NOT NULL,
    PRIMARY KEY (module, t)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS errors (
    t INTEGER NOT NULL, module TEXT NOT NULL, message TEXT NOT NULL,
    PRIMARY KEY (t, module)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS meta (t INTEGER PRIMARY KEY, data TEXT NOT NULL);
";

const ENVELOPE: [&str; 5] = ["type", "schema", "t", "time", "errors"];

/// Merges into an existing bucket, so a bucket written before a restart is
/// combined with the one written after it instead of replaced.
fn upsert_rollup(table: &str) -> String {
    format!(
        "INSERT INTO {table} (series_id, t, min, avg, max, count) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (series_id, t) DO UPDATE SET
           min = min(min, excluded.min), max = max(max, excluded.max),
           avg = (avg * count + excluded.avg * excluded.count) / (count + excluded.count),
           count = count + excluded.count"
    )
}

#[derive(Debug, Clone, Copy)]
struct Acc {
    min: f64,
    max: f64,
    sum: f64,
    count: i64,
}

impl Acc {
    fn new(v: f64) -> Self {
        Self {
            min: v,
            max: v,
            sum: v,
            count: 1,
        }
    }

    fn add(&mut self, v: f64) {
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.sum += v;
        self.count += 1;
    }
}

pub struct Db {
    conn: Connection,
    retention: Retention,
    snapshot_modules: HashSet<String>,
    series: HashMap<String, i64>,
    /// Open 1-minute buckets by (series id, bucket start); 15-minute buckets are
    /// merged in the database as each minute closes.
    minute: HashMap<(i64, i64), Acc>,
    last_prune: i64,
}

impl Db {
    pub fn open(path: &Path, retention: Retention, snapshot_modules: HashSet<String>) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        // crash evidence lives in the fsync'd JSONL; the database may lose its last commits on power loss
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        let series = conn
            .prepare("SELECT path, id FROM series")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let minute = reopen_minutes(&conn, &series)?;
        Ok(Self {
            conn,
            retention,
            snapshot_modules,
            series,
            minute,
            last_prune: 0,
        })
    }

    pub fn write(&mut self, record: &Value) -> Result<()> {
        let t = record["t"].as_f64().context("record has no t")?.round() as i64;
        let tx = self.conn.transaction()?;
        match record["type"].as_str() {
            Some("meta") => {
                tx.execute(
                    "INSERT OR REPLACE INTO meta (t, data) VALUES (?1, ?2)",
                    params![t, record.to_string()],
                )?;
            }
            Some("sample") => {
                close_minutes(&tx, &mut self.minute, t - t.rem_euclid(60))?;
                write_sample(
                    &tx,
                    &mut self.series,
                    &mut self.minute,
                    &self.snapshot_modules,
                    t,
                    record,
                )?;
            }
            other => anyhow::bail!("unknown record type {other:?}"),
        }
        if t - self.last_prune >= 3600 {
            prune(&tx, &self.retention, t)?;
            self.last_prune = t;
        }
        tx.commit()?;
        Ok(())
    }
}

fn write_sample(
    tx: &Transaction,
    series: &mut HashMap<String, i64>,
    minute: &mut HashMap<(i64, i64), Acc>,
    snapshot_modules: &HashSet<String>,
    t: i64,
    record: &Value,
) -> Result<()> {
    let Some(fields) = record.as_object() else {
        return Ok(());
    };
    let mut points = Vec::new();
    for (name, value) in fields.iter().filter(|(k, _)| !ENVELOPE.contains(&k.as_str())) {
        flatten(value, &mut format!("/{}", escape(name)), &mut points);
        if snapshot_modules.contains(name) {
            tx.execute(
                "INSERT OR REPLACE INTO snapshots (module, t, data) VALUES (?1, ?2, ?3)",
                params![name, t, value.to_string()],
            )?;
        }
    }
    if let Some(errors) = record.get("errors").and_then(Value::as_object) {
        for (module, message) in errors {
            tx.execute(
                "INSERT OR REPLACE INTO errors (t, module, message) VALUES (?1, ?2, ?3)",
                params![t, module, message.as_str().unwrap_or_default()],
            )?;
        }
    }

    let mut insert = tx.prepare_cached("INSERT OR REPLACE INTO points (series_id, t, value) VALUES (?1, ?2, ?3)")?;
    for (path, v) in points {
        let id = match series.get(&path) {
            Some(id) => *id,
            None => {
                tx.execute("INSERT INTO series (path) VALUES (?1)", params![path])?;
                let id = tx.last_insert_rowid();
                series.insert(path, id);
                id
            }
        };
        insert.execute(params![id, t, v])?;
        add_point(minute, id, t, v);
    }
    Ok(())
}

fn add_point(minute: &mut HashMap<(i64, i64), Acc>, id: i64, t: i64, v: f64) {
    minute
        .entry((id, t - t.rem_euclid(60)))
        .and_modify(|a| a.add(v))
        .or_insert_with(|| Acc::new(v));
}

/// Rebuilds the minutes a previous run left open from their raw points, so a
/// restart does not drop them from the rollups.
fn reopen_minutes(conn: &Connection, series: &HashMap<String, i64>) -> Result<HashMap<(i64, i64), Acc>> {
    let mut minute = HashMap::new();
    let mut last = conn.prepare("SELECT max(t) FROM rollup_1m WHERE series_id = ?1")?;
    let mut points = conn.prepare("SELECT t, value FROM points WHERE series_id = ?1 AND t >= ?2")?;
    for &id in series.values() {
        let rolled: Option<i64> = last.query_row([id], |r| r.get(0))?;
        let from = rolled.map_or(i64::MIN, |t| t + 60);
        for row in points.query_map(params![id, from], |r| Ok((r.get(0)?, r.get(1)?)))? {
            let (t, v) = row?;
            add_point(&mut minute, id, t, v);
        }
    }
    Ok(minute)
}

/// Writes every 1-minute bucket older than `current` and merges it into its 15-minute bucket.
fn close_minutes(tx: &Transaction, minute: &mut HashMap<(i64, i64), Acc>, current: i64) -> Result<()> {
    let closed: Vec<(i64, i64)> = minute.keys().filter(|(_, t)| *t < current).copied().collect();
    if closed.is_empty() {
        return Ok(());
    }
    let mut m1 = tx.prepare_cached(&upsert_rollup("rollup_1m"))?;
    let mut m15 = tx.prepare_cached(&upsert_rollup("rollup_15m"))?;
    for (id, t) in closed {
        let a = minute.remove(&(id, t)).expect("listed above");
        let avg = a.sum / a.count as f64;
        m1.execute(params![id, t, a.min, avg, a.max, a.count])?;
        m15.execute(params![id, t - t.rem_euclid(900), a.min, avg, a.max, a.count])?;
    }
    Ok(())
}

fn prune(tx: &Transaction, r: &Retention, now: i64) -> Result<()> {
    let raw = now - i64::from(r.raw_hours) * 3600;
    let ids: Vec<i64> = tx
        .prepare("SELECT id FROM series")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    // per-series range deletes use the primary key; a bare `t < ?` would scan the whole table
    let mut points = tx.prepare_cached("DELETE FROM points WHERE series_id = ?1 AND t < ?2")?;
    let mut m1 = tx.prepare_cached("DELETE FROM rollup_1m WHERE series_id = ?1 AND t < ?2")?;
    let mut m15 = tx.prepare_cached("DELETE FROM rollup_15m WHERE series_id = ?1 AND t < ?2")?;
    for id in ids {
        points.execute(params![id, raw])?;
        m1.execute(params![id, now - i64::from(r.rollup_1m_days) * 86400])?;
        m15.execute(params![id, now - i64::from(r.rollup_15m_days) * 86400])?;
    }
    let modules: Vec<String> = tx
        .prepare("SELECT DISTINCT module FROM snapshots")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for module in modules {
        tx.execute(
            "DELETE FROM snapshots WHERE module = ?1 AND t < ?2",
            params![module, raw],
        )?;
    }
    tx.execute("DELETE FROM errors WHERE t < ?1", params![raw])?;
    Ok(())
}

/// Numeric leaves keyed by JSON pointer. Arrays of objects are skipped: a
/// ranked list's index is not an identity, so it cannot be a series.
fn flatten(value: &Value, path: &mut String, out: &mut Vec<(String, f64)>) {
    match value {
        Value::Number(n) => {
            if let Some(v) = n.as_f64() {
                out.push((path.clone(), v));
            }
        }
        Value::Bool(b) => out.push((path.clone(), f64::from(u8::from(*b)))),
        Value::Object(map) => {
            for (k, v) in map {
                let len = path.len();
                path.push('/');
                path.push_str(&escape(k));
                flatten(v, path, out);
                path.truncate(len);
            }
        }
        Value::Array(items) if items.iter().all(|i| i.is_number() || i.is_null()) => {
            for (i, v) in items.iter().enumerate() {
                let len = path.len();
                path.push('/');
                path.push_str(&i.to_string());
                flatten(v, path, out);
                path.truncate(len);
            }
        }
        Value::Array(_) | Value::String(_) | Value::Null => {}
    }
}

/// RFC 6901: chip keys such as `k10temp/0000:00:18.3` contain `/`.
fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Runs the database on its own thread behind a bounded queue, so a slow disk
/// or a long prune never delays the crash log.
pub struct Writer {
    tx: SyncSender<Value>,
    dropped: u64,
}

impl Writer {
    pub fn spawn(db: Db) -> Self {
        let (tx, rx) = mpsc::sync_channel(64);
        thread::Builder::new()
            .name("db".into())
            .spawn(move || serve(db, rx))
            .expect("spawning the db thread");
        Self { tx, dropped: 0 }
    }

    pub fn send(&mut self, record: Value) {
        match self.tx.try_send(record) {
            Ok(()) if self.dropped > 0 => {
                eprintln!("db: queue drained; {} records were dropped", self.dropped);
                self.dropped = 0;
            }
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                if self.dropped == 0 {
                    eprintln!("db: writer is behind; dropping records until it catches up");
                }
                self.dropped += 1;
            }
            Err(TrySendError::Disconnected(_)) => {
                if self.dropped == 0 {
                    eprintln!("db: writer thread has stopped; history is not being recorded");
                }
                self.dropped += 1;
            }
        }
    }
}

fn serve(mut db: Db, rx: Receiver<Value>) {
    let mut last_error = None;
    for record in rx {
        if let Err(e) = db.write(&record) {
            let msg = format!("{e:#}");
            if last_error.as_ref() != Some(&msg) {
                eprintln!("db: {msg}");
                last_error = Some(msg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn open(dir: &tempfile::TempDir) -> Db {
        let snapshots = HashSet::from(["processes".to_string()]);
        Db::open(&dir.path().join("h.db"), Retention::default(), snapshots).unwrap()
    }

    fn sample(t: i64, temp: f64) -> Value {
        json!({"type": "sample", "schema": 3, "t": t as f64, "time": "x",
               "hwmon": {"k10temp/0000:00:18.3": {"temp": {"Tctl": temp}}}})
    }

    fn rollup(db: &Db, table: &str) -> Vec<(i64, f64, f64, f64, i64)> {
        db.conn
            .prepare(&format!("SELECT t, min, avg, max, count FROM {table} ORDER BY t"))
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn series_paths_escape_slashes_in_keys() {
        let mut out = Vec::new();
        flatten(
            &json!({"k10temp/0000:00:18.3": {"a~b": 1}}),
            &mut "/hwmon".into(),
            &mut out,
        );
        assert_eq!(out, [("/hwmon/k10temp~10000:00:18.3/a~0b".to_string(), 1.0)]);
    }

    #[test]
    fn ranked_lists_are_not_series_but_number_arrays_are() {
        let mut out = Vec::new();
        let v = json!({"core_pct": [5, null, 7], "top_cpu": [{"pid": 1, "cpu_pct": 50}], "name": "x", "on": true});
        flatten(&v, &mut "/m".into(), &mut out);
        let paths: Vec<_> = out.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, ["/m/core_pct/0", "/m/core_pct/2", "/m/on"]);
    }

    #[test]
    fn minute_rollup_is_written_when_the_minute_closes() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        for (t, v) in [(120, 40.0), (122, 50.0), (178, 60.0)] {
            db.write(&sample(t, v)).unwrap();
        }
        assert!(rollup(&db, "rollup_1m").is_empty(), "the minute is still open");
        db.write(&sample(180, 70.0)).unwrap();
        assert_eq!(rollup(&db, "rollup_1m"), [(120, 40.0, 50.0, 60.0, 3)]);
        assert_eq!(rollup(&db, "rollup_15m"), [(0, 40.0, 50.0, 60.0, 3)]);
    }

    #[test]
    fn restart_merges_into_existing_buckets_instead_of_replacing_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.write(&sample(120, 10.0)).unwrap();
        db.write(&sample(180, 20.0)).unwrap();
        drop(db);
        // the minute open at shutdown (t=180) must still reach the rollups, and the second
        // process's buckets must merge with the first's instead of replacing them
        let mut db = open(&dir);
        db.write(&sample(240, 30.0)).unwrap();
        db.write(&sample(300, 99.0)).unwrap();
        assert_eq!(rollup(&db, "rollup_15m"), [(0, 10.0, 20.0, 30.0, 3)]);
    }

    #[test]
    fn snapshots_only_for_declared_modules_and_errors_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        let rec = json!({"type": "sample", "schema": 3, "t": 100.0, "time": "x",
                         "processes": {"count": 3, "top_cpu": [{"pid": 1}]}, "cpu": {"total_pct": 5},
                         "errors": {"gpu": "timeout"}});
        db.write(&rec).unwrap();
        let modules: Vec<String> = db
            .conn
            .prepare("SELECT module FROM snapshots")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(modules, ["processes"]);
        let msg: String = db
            .conn
            .query_row("SELECT message FROM errors WHERE module='gpu'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msg, "timeout");
    }

    #[test]
    fn prune_drops_raw_points_but_keeps_rollups() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.write(&sample(120, 1.0)).unwrap();
        db.write(&sample(180, 2.0)).unwrap();
        // 49 h later: past raw retention (48 h), well inside rollup retention
        db.write(&sample(180 + 49 * 3600, 3.0)).unwrap();
        let points: i64 = db
            .conn
            .query_row("SELECT count(*) FROM points", [], |r| r.get(0))
            .unwrap();
        assert_eq!(points, 1);
        assert_eq!(rollup(&db, "rollup_1m").len(), 2);
    }
}
