use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use schema::db::{ADDED_COLUMNS, AFTER_ADDED, SCHEMA};
use schema::processes::Process;
use serde::Deserialize;
use serde_json::Value;

use schema::series::{escape, flatten};

use crate::config::Retention;
use crate::events::{Event, crash_summary};

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

/// One process's readings in a tick; `fds` and the rates are absent for processes that cannot be inspected.
#[derive(Debug, Clone, Copy)]
struct ProcPoint {
    cpu_pct: f64,
    rss_bytes: i64,
    threads: i64,
    fds: Option<i64>,
    read_bps: Option<f64>,
    write_bps: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct ProcAcc {
    cpu_sum: f64,
    cpu_max: f64,
    rss_sum: f64,
    rss_max: i64,
    threads_max: i64,
    fds_max: Option<i64>,
    read_sum: f64,
    read_n: i64,
    write_sum: f64,
    write_n: i64,
    count: i64,
}

impl ProcAcc {
    fn new(p: ProcPoint) -> Self {
        let mut a = Self {
            cpu_sum: 0.0,
            cpu_max: p.cpu_pct,
            rss_sum: 0.0,
            rss_max: p.rss_bytes,
            threads_max: p.threads,
            fds_max: None,
            read_sum: 0.0,
            read_n: 0,
            write_sum: 0.0,
            write_n: 0,
            count: 0,
        };
        a.add(p);
        a
    }

    fn add(&mut self, p: ProcPoint) {
        self.cpu_sum += p.cpu_pct;
        self.cpu_max = self.cpu_max.max(p.cpu_pct);
        self.rss_sum += p.rss_bytes as f64;
        self.rss_max = self.rss_max.max(p.rss_bytes);
        self.threads_max = self.threads_max.max(p.threads);
        self.fds_max = self.fds_max.max(p.fds);
        if let Some(r) = p.read_bps {
            self.read_sum += r;
            self.read_n += 1;
        }
        if let Some(w) = p.write_bps {
            self.write_sum += w;
            self.write_n += 1;
        }
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
    /// (pid, start time) to (id, last_t); only processes seen within raw retention are kept.
    procs: HashMap<(i64, i64), (i64, i64)>,
    /// Open 1-minute process buckets by (proc id, bucket start).
    proc_minute: HashMap<(i64, i64), ProcAcc>,
    last_prune: i64,
}

impl Db {
    pub fn open(path: &Path, retention: Retention, snapshot_modules: HashSet<String>) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        // it holds what other users' programs and units did; SQLite gives -wal and -shm the database's mode
        owner_only(path, true).with_context(|| format!("restricting {}", path.display()))?;
        for suffix in ["-wal", "-shm"] {
            let mut side = path.as_os_str().to_owned();
            side.push(suffix);
            owner_only(Path::new(&side), false)?;
        }
        let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        // crash evidence lives in the fsync'd JSONL; the database may lose its last commits on power loss
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrate(&mut conn).context("updating the database schema")?;
        let series = conn
            .prepare("SELECT path, id FROM series")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let minute = reopen_minutes(&conn, &series)?;
        // relative to the newest process rather than the clock, so the window follows the data
        let newest: Option<i64> = conn.query_row("SELECT max(last_t) FROM procs", [], |r| r.get(0))?;
        let procs: HashMap<(i64, i64), (i64, i64)> = match newest {
            Some(newest) => conn
                .prepare("SELECT pid, start_time, id, last_t FROM procs WHERE last_t >= ?1")?
                .query_map([newest - i64::from(retention.raw_hours) * 3600], |r| {
                    Ok(((r.get(0)?, r.get(1)?), (r.get(2)?, r.get(3)?)))
                })?
                .collect::<rusqlite::Result<_>>()?,
            None => HashMap::new(),
        };
        let proc_minute = reopen_proc_minutes(&conn, &procs)?;
        Ok(Self {
            conn,
            retention,
            snapshot_modules,
            series,
            minute,
            procs,
            proc_minute,
            last_prune: 0,
        })
    }

    #[cfg(test)]
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// The events thread's resume point, as it last wrote it.
    pub fn events_state(&self) -> Result<HashMap<String, String>> {
        Ok(self
            .conn
            .prepare("SELECT name, value FROM events_state")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
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
                close_proc_minutes(&tx, &mut self.proc_minute, t - t.rem_euclid(60))?;
                write_sample(
                    &tx,
                    &mut self.series,
                    &mut self.minute,
                    &self.snapshot_modules,
                    t,
                    record,
                )?;
                if let Some(table) = record["processes"].get("table") {
                    write_procs(
                        &tx,
                        &mut self.procs,
                        &mut self.proc_minute,
                        t,
                        Vec::<Process>::deserialize(table)?,
                    )?;
                }
            }
            Some("events") => write_events(&tx, t, record)?,
            other => anyhow::bail!("unknown record type {other:?}"),
        }
        if t - self.last_prune >= 3600 {
            prune(&tx, &self.retention, t)?;
            let raw = t - i64::from(self.retention.raw_hours) * 3600;
            self.procs.retain(|_, (_, last)| *last >= raw);
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
                params![name, t, snapshot_json(name, value)],
            )?;
        }
    }
    write_errors(tx, t, record)?;

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

fn write_errors(tx: &Transaction, t: i64, record: &Value) -> Result<()> {
    if let Some(errors) = record.get("errors").and_then(Value::as_object) {
        for (module, message) in errors {
            tx.execute(
                "INSERT OR REPLACE INTO errors (t, module, message) VALUES (?1, ?2, ?3)",
                params![t, module, message.as_str().unwrap_or_default()],
            )?;
        }
    }
    Ok(())
}

/// Events and the resume point they were read up to commit together, so a crash between them cannot skip events.
fn write_events(tx: &Transaction, t: i64, record: &Value) -> Result<()> {
    // A fold read again as it grows (a unit failing again, a loop's next restart, a transaction still running)
    // refines its row; a plain re-read writes the same values.
    let mut insert = tx.prepare_cached(
        "INSERT INTO events (t, kind, key, boot_id, summary, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (t, kind, key) DO UPDATE SET
           boot_id = excluded.boot_id, summary = excluded.summary, data = excluded.data",
    )?;
    for e in Vec::<Event>::deserialize(&record["events"])? {
        if e.kind == "crash" {
            fold_crash(tx, &e)?;
        } else {
            insert.execute(params![e.t, e.kind, e.key, e.boot_id, e.summary, e.data.to_string()])?;
        }
    }
    if let Some(state) = record.get("state").and_then(Value::as_object) {
        for (name, value) in state {
            match value.as_str() {
                Some(v) => tx.execute(
                    "INSERT OR REPLACE INTO events_state (name, value) VALUES (?1, ?2)",
                    params![name, v],
                )?,
                None => tx.execute("DELETE FROM events_state WHERE name = ?1", params![name])?,
            };
        }
    }
    write_errors(tx, t, record)
}

/// `table` feeds the process history; left in, every snapshot row would carry the whole process list.
/// A program's crashes in one boot share a row, so a crash loop is one row whose `count` grows. A crash no later than
/// the newest one counted was read before (a resume from earlier) and is not counted again.
fn fold_crash(tx: &Transaction, e: &Event) -> Result<()> {
    let row: Option<(i64, String)> = tx
        .prepare_cached("SELECT t, data FROM events WHERE kind = 'crash' AND key = ?1 AND boot_id IS ?2")?
        .query_row(params![e.key, e.boot_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let Some((t, old)) = row else {
        tx.prepare_cached("INSERT INTO events (t, kind, key, boot_id, summary, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
            .execute(params![e.t, e.kind, e.key, e.boot_id, e.summary, e.data.to_string()])?;
        return Ok(());
    };
    let old: Value = serde_json::from_str(&old).unwrap_or_default();
    let seen = old["last_us"].as_u64().unwrap_or(0);
    if e.data["last_us"].as_u64().is_none_or(|us| us <= seen) {
        return Ok(());
    }
    let count = old["count"].as_u64().unwrap_or(1) + 1;
    let mut data = e.data.clone();
    data["count"] = count.into();
    let signal = data["signal"].as_str().unwrap_or("unknown signal");
    let summary = crash_summary(&e.key, signal, count);
    tx.prepare_cached(
        "UPDATE events SET summary = ?1, data = ?2 WHERE t = ?3 AND kind = 'crash' AND key = ?4 AND boot_id IS ?5",
    )?
    .execute(params![summary, data.to_string(), t, e.key, e.boot_id])?;
    Ok(())
}

/// Creates what is missing: tables, then the columns added since, which `CREATE TABLE IF NOT EXISTS` leaves out of a
/// table that exists. One transaction, so a failure leaves the database as it was.
fn migrate(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA)?;
    let mut has = tx.prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2")?;
    for (table, column, kind) in ADDED_COLUMNS {
        if !has.exists([table, column])? {
            tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
        }
    }
    drop(has);
    tx.execute_batch(AFTER_ADDED)?;
    tx.commit()?;
    Ok(())
}

/// Creates `path` (when `create`) or narrows an existing one to mode 0600.
fn owner_only(path: &Path, create: bool) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if create {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
    }
    match std::fs::metadata(path) {
        Ok(m) if m.permissions().mode() & 0o077 != 0 => {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    Ok(())
}

fn snapshot_json(module: &str, value: &Value) -> String {
    match value.as_object() {
        Some(fields) if module == "processes" && fields.contains_key("table") => {
            let kept = fields.iter().filter(|(k, _)| *k != "table");
            Value::Object(kept.map(|(k, v)| (k.clone(), v.clone())).collect()).to_string()
        }
        _ => value.to_string(),
    }
}

fn write_procs(
    tx: &Transaction,
    procs: &mut HashMap<(i64, i64), (i64, i64)>,
    minute: &mut HashMap<(i64, i64), ProcAcc>,
    t: i64,
    rows: Vec<Process>,
) -> Result<()> {
    // a row recorded before ppid and cgroup were gets them once, from the first sample after the upgrade that has them
    let mut touch = tx.prepare_cached(
        "UPDATE procs SET last_t = ?2, ppid = coalesce(ppid, ?3), cgroup_id = coalesce(cgroup_id, ?4) WHERE id = ?1",
    )?;
    // the conflict branch covers an instance whose id was not loaded into the map at open
    let mut insert = tx.prepare_cached(
        "INSERT INTO procs (pid, start_time, comm, cmdline, user, first_t, last_t, ppid, cgroup_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8)
         ON CONFLICT (pid, start_time) DO UPDATE SET last_t = excluded.last_t,
           ppid = coalesce(ppid, excluded.ppid), cgroup_id = coalesce(cgroup_id, excluded.cgroup_id)
         RETURNING id",
    )?;
    let mut point = tx.prepare_cached(
        "INSERT OR REPLACE INTO proc_points (proc_id, t, cpu_pct, rss_bytes, threads, fds, read_bps, write_bps)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for r in rows {
        let key = (i64::from(r.pid), r.start_time as i64);
        let cgroup = r.cgroup.as_deref().map(|p| intern_cgroup(tx, p)).transpose()?;
        let id = match procs.get_mut(&key) {
            Some((id, last)) => {
                touch.execute(params![*id, t, r.ppid, cgroup])?;
                *last = t;
                *id
            }
            None => {
                let cmdline = Some(r.cmdline.as_str()).filter(|c| !c.is_empty());
                let id = insert.query_row(
                    params![key.0, key.1, r.comm, cmdline, r.user, t, r.ppid, cgroup],
                    |row| row.get(0),
                )?;
                procs.insert(key, (id, t));
                id
            }
        };
        let p = ProcPoint {
            cpu_pct: r.cpu_pct,
            rss_bytes: r.rss_bytes as i64,
            threads: r.threads as i64,
            fds: r.fds.map(|f| f as i64),
            read_bps: r.read_bps.map(|v| v as f64),
            write_bps: r.write_bps.map(|v| v as f64),
        };
        point.execute(params![
            id,
            t,
            p.cpu_pct,
            p.rss_bytes,
            p.threads,
            p.fds,
            p.read_bps,
            p.write_bps
        ])?;
        add_proc_point(minute, id, t, p);
    }
    Ok(())
}

fn intern_cgroup(tx: &Transaction, path: &str) -> Result<i64> {
    let found = tx
        .prepare_cached("SELECT id FROM cgroups WHERE path = ?1")?
        .query_row([path], |r| r.get(0))
        .optional()?;
    if let Some(id) = found {
        return Ok(id);
    }
    tx.prepare_cached("INSERT INTO cgroups (path) VALUES (?1)")?
        .execute([path])?;
    Ok(tx.last_insert_rowid())
}

fn add_proc_point(minute: &mut HashMap<(i64, i64), ProcAcc>, id: i64, t: i64, p: ProcPoint) {
    minute
        .entry((id, t - t.rem_euclid(60)))
        .and_modify(|a| a.add(p))
        .or_insert_with(|| ProcAcc::new(p));
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

/// Rebuilds the process minutes a previous run left open, as `reopen_minutes` does for series.
fn reopen_proc_minutes(
    conn: &Connection,
    procs: &HashMap<(i64, i64), (i64, i64)>,
) -> Result<HashMap<(i64, i64), ProcAcc>> {
    let mut minute = HashMap::new();
    let mut last = conn.prepare("SELECT max(t) FROM proc_rollup_1m WHERE proc_id = ?1")?;
    let mut points = conn.prepare(
        "SELECT t, cpu_pct, rss_bytes, threads, fds, read_bps, write_bps FROM proc_points WHERE proc_id = ?1 AND t >= ?2",
    )?;
    for &(id, _) in procs.values() {
        let rolled: Option<i64> = last.query_row([id], |r| r.get(0))?;
        let from = rolled.map_or(i64::MIN, |t| t + 60);
        let rows = points.query_map(params![id, from], |r| {
            let p = ProcPoint {
                cpu_pct: r.get(1)?,
                rss_bytes: r.get(2)?,
                threads: r.get(3)?,
                fds: r.get(4)?,
                read_bps: r.get(5)?,
                write_bps: r.get(6)?,
            };
            Ok((r.get(0)?, p))
        })?;
        for row in rows {
            let (t, p) = row?;
            add_proc_point(&mut minute, id, t, p);
        }
    }
    Ok(minute)
}

/// Writes every 1-minute process bucket older than `current`.
fn close_proc_minutes(tx: &Transaction, minute: &mut HashMap<(i64, i64), ProcAcc>, current: i64) -> Result<()> {
    let closed: Vec<(i64, i64)> = minute.keys().filter(|(_, t)| *t < current).copied().collect();
    if closed.is_empty() {
        return Ok(());
    }
    // a minute is rebuilt whole from its points on restart, so a replace never loses a half
    let mut insert = tx.prepare_cached(
        "INSERT OR REPLACE INTO proc_rollup_1m (proc_id, t, cpu_avg, cpu_max, rss_avg, rss_max, threads_max,
                                               fds_max, read_bps_avg, write_bps_avg, count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    for (id, t) in closed {
        let a = minute.remove(&(id, t)).expect("listed above");
        let n = a.count as f64;
        let read = (a.read_n > 0).then(|| a.read_sum / a.read_n as f64);
        let write = (a.write_n > 0).then(|| a.write_sum / a.write_n as f64);
        insert.execute(params![
            id,
            t,
            a.cpu_sum / n,
            a.cpu_max,
            a.rss_sum / n,
            a.rss_max,
            a.threads_max,
            a.fds_max,
            read,
            write,
            a.count
        ])?;
    }
    Ok(())
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
    tx.execute(
        "DELETE FROM events WHERE t < ?1",
        params![now - i64::from(r.events_days) * 86400],
    )?;
    prune_procs(tx, r, now)
}

fn prune_procs(tx: &Transaction, r: &Retention, now: i64) -> Result<()> {
    let raw = now - i64::from(r.raw_hours) * 3600;
    let cutoff = now - i64::from(r.proc_rollup_days) * 86400;
    let ids = |sql: &str| -> Result<Vec<i64>> {
        Ok(tx
            .prepare_cached(sql)?
            .query_map([cutoff], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    };
    // per-process range deletes use the primary key; a bare `t < ?` would scan the whole table
    let mut points = tx.prepare_cached("DELETE FROM proc_points WHERE proc_id = ?1 AND t < ?2")?;
    let mut rollups = tx.prepare_cached("DELETE FROM proc_rollup_1m WHERE proc_id = ?1 AND t < ?2")?;
    for id in ids("SELECT id FROM procs WHERE last_t >= ?1")? {
        points.execute(params![id, raw])?;
        rollups.execute(params![id, cutoff])?;
    }
    for id in ids("SELECT id FROM procs WHERE last_t < ?1")? {
        points.execute(params![id, i64::MAX])?;
        rollups.execute(params![id, i64::MAX])?;
    }
    if tx.execute("DELETE FROM procs WHERE last_t < ?1", params![cutoff])? > 0 {
        // probes the procs_cgroup index once per distinct path
        tx.execute(
            "DELETE FROM cgroups WHERE NOT EXISTS (SELECT 1 FROM procs WHERE cgroup_id = cgroups.id)",
            [],
        )?;
    }
    Ok(())
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

    /// For a producer that may wait for room in the queue rather than drop.
    pub fn sender(&self) -> SyncSender<Value> {
        self.tx.clone()
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

    #[test]
    fn the_database_and_its_wal_are_readable_by_the_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = |suffix: &str| dir.path().join(format!("h.db{suffix}"));
        let mode = |suffix: &str| std::fs::metadata(path(suffix)).unwrap().permissions().mode() & 0o777;
        let mut db = open(&dir);
        db.write(&sample(60, 40.0)).unwrap();
        assert_eq!(
            [mode(""), mode("-wal"), mode("-shm")],
            [0o600; 3],
            "it names other users' crashing programs, failed units and package commands"
        );
        drop(db);

        // as an older daemon, killed while running, left them
        for suffix in ["", "-wal", "-shm"] {
            if !path(suffix).exists() {
                std::fs::write(path(suffix), "").unwrap();
            }
            std::fs::set_permissions(path(suffix), std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let _db = open(&dir);
        assert_eq!(
            [mode(""), mode("-wal"), mode("-shm")],
            [0o600; 3],
            "world-readable files are narrowed on open"
        );
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

    fn proc_row(pid: u32, start: u64, cpu: f64) -> Value {
        json!({"pid": pid, "ppid": 1, "comm": "p", "state": "S", "threads": 2, "rss_bytes": 1000, "cpu_pct": cpu,
               "read_bps": 10, "fds": 3, "cmdline": "p --x", "user": "me", "start_time": start})
    }

    fn proc_sample(t: i64, rows: Vec<Value>) -> Value {
        json!({"type": "sample", "schema": 3, "t": t as f64, "time": "x",
               "processes": {"count": rows.len(), "top_cpu": [], "table": rows}})
    }

    fn count(db: &Db, table: &str) -> i64 {
        db.conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn reused_pid_with_a_new_start_time_is_a_separate_process() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.write(&proc_sample(100, vec![proc_row(7, 50, 1.0)])).unwrap();
        db.write(&proc_sample(110, vec![proc_row(7, 50, 1.0), proc_row(8, 60, 1.0)]))
            .unwrap();
        db.write(&proc_sample(120, vec![proc_row(7, 90, 5.0)])).unwrap();
        assert_eq!(count(&db, "procs"), 3);
        // the old instance keeps only its own two points; the pid's new owner starts clean
        let per_proc: Vec<(i64, i64)> = db
            .conn
            .prepare("SELECT p.start_time, count(*) FROM proc_points pp JOIN procs p ON p.id = pp.proc_id WHERE p.pid = 7 GROUP BY p.id ORDER BY p.start_time")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(per_proc, [(50, 2), (90, 1)]);
        let (first, last): (i64, i64) = db
            .conn
            .query_row(
                "SELECT first_t, last_t FROM procs WHERE pid = 7 AND start_time = 50",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((first, last), (100, 110));
    }

    #[test]
    fn proc_minutes_survive_a_restart_mid_minute() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.write(&proc_sample(120, vec![proc_row(7, 50, 10.0)])).unwrap();
        db.write(&proc_sample(130, vec![proc_row(7, 50, 30.0)])).unwrap();
        assert_eq!(count(&db, "proc_rollup_1m"), 0, "the minute is still open");
        drop(db);
        // the minute open at shutdown must be rebuilt from its points, not dropped
        let mut db = open(&dir);
        db.write(&proc_sample(140, vec![proc_row(7, 50, 20.0)])).unwrap();
        db.write(&proc_sample(180, vec![proc_row(7, 50, 0.0)])).unwrap();
        let (t, avg, max, rss, fds, read, write, n): (i64, f64, f64, f64, i64, f64, Option<f64>, i64) = db
            .conn
            .query_row(
                "SELECT t, cpu_avg, cpu_max, rss_avg, fds_max, read_bps_avg, write_bps_avg, count FROM proc_rollup_1m",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            (t, avg, max, rss, fds, read, write, n),
            (120, 20.0, 30.0, 1000.0, 3, 10.0, None, 3)
        );
        assert_eq!(count(&db, "proc_points"), 4);
    }

    #[test]
    fn retention_removes_old_proc_points_rollups_and_procs() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.write(&proc_sample(120, vec![proc_row(7, 50, 1.0)])).unwrap();
        db.write(&proc_sample(180, vec![proc_row(7, 50, 1.0)])).unwrap();
        // 49 h later: past raw retention (48 h), inside the 7 days of rollups
        let later = 180 + 49 * 3600;
        db.write(&proc_sample(later, vec![proc_row(8, 60, 1.0)])).unwrap();
        assert_eq!(
            (
                count(&db, "proc_points"),
                count(&db, "proc_rollup_1m"),
                count(&db, "procs")
            ),
            (1, 2, 2)
        );
        // 8 days later: the first process is past every window
        db.write(&proc_sample(180 + 8 * 86400, vec![proc_row(8, 60, 1.0)]))
            .unwrap();
        assert_eq!(
            (
                count(&db, "proc_points"),
                count(&db, "proc_rollup_1m"),
                count(&db, "procs")
            ),
            (1, 1, 1)
        );
        assert_eq!(db.procs.len(), 1, "the in-memory map is bounded too");
    }

    /// A row as the collector hands it over on the sample that first reports the instance.
    fn first_seen(pid: u32, start: u64, ppid: u32, cgroup: &str) -> Value {
        let mut row = proc_row(pid, start, 1.0);
        row["ppid"] = ppid.into();
        row["cgroup"] = cgroup.into();
        row
    }

    /// (ppid, cgroup path) of the instance.
    fn origin(db: &Db, pid: u32, start: i64) -> (Option<i64>, Option<String>) {
        db.conn
            .query_row(
                "SELECT p.ppid, c.path FROM procs p LEFT JOIN cgroups c ON c.id = p.cgroup_id
                 WHERE p.pid = ?1 AND p.start_time = ?2",
                params![pid, start],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    #[test]
    fn a_new_instance_keeps_the_cgroup_and_parent_it_was_first_seen_with() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.write(&proc_sample(
            100,
            vec![first_seen(7, 50, 300, "/system.slice/a.service")],
        ))
        .unwrap();
        // later samples carry no cgroup, and the parent may have exited and left the process to init
        let mut reparented = proc_row(7, 50, 1.0);
        reparented["ppid"] = 1.into();
        db.write(&proc_sample(110, vec![reparented])).unwrap();
        assert_eq!(
            origin(&db, 7, 50),
            (Some(300), Some("/system.slice/a.service".into())),
            "why it ran is answered by how it started"
        );
    }

    #[test]
    fn instances_in_the_same_cgroup_share_one_path_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        let unit = "/user.slice/user-1000.slice/user@1000.service/app.slice/a.service";
        db.write(&proc_sample(
            100,
            vec![first_seen(7, 50, 1, unit), first_seen(8, 51, 7, unit)],
        ))
        .unwrap();
        db.write(&proc_sample(110, vec![first_seen(9, 52, 7, unit)])).unwrap();
        assert_eq!(
            count(&db, "cgroups"),
            1,
            "a path is stored once, however many processes ran in it"
        );
        let ids: i64 = db
            .conn
            .query_row("SELECT count(DISTINCT cgroup_id) FROM procs", [], |r| r.get(0))
            .unwrap();
        assert_eq!((count(&db, "procs"), ids), (3, 1));
    }

    #[test]
    fn retention_drops_paths_no_process_refers_to_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.write(&proc_sample(
            100,
            vec![
                first_seen(7, 50, 1, "/system.slice/gone.service"),
                first_seen(8, 51, 1, "/system.slice/shared.service"),
            ],
        ))
        .unwrap();
        let later = 100 + 2 * 86400;
        db.write(&proc_sample(
            later,
            vec![
                first_seen(9, 60, 1, "/system.slice/shared.service"),
                first_seen(10, 61, 1, "/system.slice/live.service"),
            ],
        ))
        .unwrap();
        // 8 days on: pids 7 and 8 are past the 7 days of process history, 9 and 10 are not
        db.write(&proc_sample(100 + 8 * 86400, vec![])).unwrap();
        let paths: Vec<String> = db
            .conn
            .prepare("SELECT path FROM cgroups ORDER BY path")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(paths, ["/system.slice/live.service", "/system.slice/shared.service"]);
        assert_eq!(origin(&db, 9, 60).1.as_deref(), Some("/system.slice/shared.service"));
    }

    /// `procs` as the release before cgroups were recorded created it.
    const OLD_PROCS: &str = "
        CREATE TABLE procs (
            id INTEGER PRIMARY KEY, pid INTEGER NOT NULL, start_time INTEGER NOT NULL, comm TEXT NOT NULL,
            cmdline TEXT, user TEXT, first_t INTEGER NOT NULL, last_t INTEGER NOT NULL,
            UNIQUE (pid, start_time));
        CREATE INDEX procs_comm ON procs (comm);
        CREATE INDEX procs_last ON procs (last_t);
        INSERT INTO procs (pid, start_time, comm, cmdline, user, first_t, last_t)
            VALUES (7, 50, 'old', 'old --x', 'me', 100, 200);";

    #[test]
    fn an_existing_database_is_migrated_in_place_keeping_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        Connection::open(dir.path().join("h.db"))
            .unwrap()
            .execute_batch(OLD_PROCS)
            .unwrap();
        let mut db = open(&dir);
        let row: (String, i64, i64, Option<i64>, Option<i64>) = db
            .conn
            .query_row(
                "SELECT comm, first_t, last_t, ppid, cgroup_id FROM procs WHERE pid = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            ("old".into(), 100, 200, None, None),
            "history recorded before the upgrade is kept, with nothing invented for it"
        );
        db.write(&proc_sample(300, vec![first_seen(8, 60, 1, "/init.scope")]))
            .unwrap();
        drop(db);
        // every start runs the migration; a second must change nothing and lose nothing
        let db = open(&dir);
        assert_eq!(count(&db, "procs"), 2);
        assert_eq!(origin(&db, 8, 60), (Some(1), Some("/init.scope".into())));
        let fresh_dir = tempfile::tempdir().unwrap();
        let shape = |db: &Db| -> Vec<(String, String)> {
            db.conn
                .prepare("SELECT name, type FROM pragma_table_info('procs')")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        assert_eq!(shape(&db), shape(&open(&fresh_dir)), "the app reads both the same way");
    }

    #[test]
    fn a_process_running_across_the_upgrade_gets_its_cgroup_from_the_first_sample_after_it() {
        let dir = tempfile::tempdir().unwrap();
        Connection::open(dir.path().join("h.db"))
            .unwrap()
            .execute_batch(OLD_PROCS)
            .unwrap();
        let mut db = open(&dir);
        // a restarted collector reports every running instance as first seen
        db.write(&proc_sample(
            300,
            vec![first_seen(7, 50, 1, "/system.slice/old.service")],
        ))
        .unwrap();
        assert_eq!(origin(&db, 7, 50), (Some(1), Some("/system.slice/old.service".into())));
        db.write(&proc_sample(310, vec![first_seen(7, 50, 99, "/elsewhere")]))
            .unwrap();
        assert_eq!(
            origin(&db, 7, 50),
            (Some(1), Some("/system.slice/old.service".into())),
            "a recorded origin is never overwritten"
        );
    }

    #[test]
    fn an_instance_whose_cgroup_could_not_be_read_is_stored_with_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.write(&proc_sample(100, vec![proc_row(7, 50, 1.0)])).unwrap();
        assert_eq!(origin(&db, 7, 50), (Some(1), None));
        assert_eq!(count(&db, "cgroups"), 0);
    }

    #[test]
    fn an_events_record_writes_its_events_resume_point_and_errors_together() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        let rec = json!({"type": "events", "t": 100.0,
                         "events": [{"t": 90, "kind": "crash", "key": "c1", "boot_id": "b", "summary": "x crashed (SIGSEGV)",
                                     "data": {"pid": 7}}],
                         "state": {"cursor": "c1", "since_us": null},
                         "errors": {"events": "journal went away"}});
        db.write(&rec).unwrap();
        assert_eq!(count(&db, "events"), 1);
        assert_eq!(
            db.events_state().unwrap(),
            HashMap::from([("cursor".into(), "c1".into())])
        );
        let msg: String = db
            .conn
            .query_row("SELECT message FROM errors WHERE module = 'events'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msg, "journal went away");
        assert_eq!(count(&db, "points"), 0, "an events record is not a sample");
    }

    #[test]
    fn snapshot_row_does_not_carry_the_process_table() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.write(&proc_sample(100, vec![proc_row(7, 50, 1.0)])).unwrap();
        let data: String = db
            .conn
            .query_row("SELECT data FROM snapshots WHERE module = 'processes'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let v: Value = serde_json::from_str(&data).unwrap();
        assert!(v.get("table").is_none() && v["count"] == 1, "{data}");
        assert_eq!(count(&db, "procs"), 1, "the table still reached the history");
    }
}
