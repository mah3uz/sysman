//! The events thread: crashes, OOM kills, failed units, restart loops, boots, sleep and package transactions, read
//! from the journal and the package logs into `history.db`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use jiff::tz::TimeZone;
use journal::Entry;
use journal::categories::{INDEX_FIELDS, ids};
use journal::findings::{Detector, Finding, FindingKind};
use journal::packages::{self, Transaction};
use journal::reader::{self, Access, Boot, Fields, Filter, Journal, Seek};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::daemon::unix_now;

const STREAM_IDS: &[&str] = &[
    ids::COREDUMP,
    ids::OOMD_KILL,
    ids::UNIT_OOM,
    ids::UNIT_FAILURE,
    ids::RESTART_SCHEDULED,
    ids::SLEEP_START,
    ids::SLEEP_STOP,
];
const CHUNK: usize = 1000;
const POLL: Duration = Duration::from_secs(2);
// Kernel lines and user journals can arrive a few seconds after the time they carry.
const LATE_US: u64 = 10_000_000;
const PACKAGES_EVERY: Duration = Duration::from_secs(60);
const PACKAGE_LOGS: [&str; 3] = ["pacman.log", "dpkg.log", "apt/history.log"];
const BACKOFF_MAX: Duration = Duration::from_secs(300);
const DAY_US: u64 = 86_400_000_000;

/// One row of `events`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub t: i64,
    pub kind: String,
    pub key: String,
    pub boot_id: Option<String>,
    pub summary: String,
    pub data: Value,
}

/// Where the last run stopped, kept in `events_state`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// The last journal entry read.
    cursor: Option<String>,
    /// The earliest entry a still-open rule depends on; reading resumes there rather than after `cursor`.
    since_us: Option<u64>,
    /// The start of the newest boot recorded.
    boot_us: Option<u64>,
    /// The start of the newest package transaction recorded.
    packages_us: Option<u64>,
}

impl State {
    pub fn from_rows(rows: &HashMap<String, String>) -> State {
        let us = |name: &str| rows.get(name).and_then(|v| v.parse().ok());
        State {
            cursor: rows.get("cursor").cloned(),
            since_us: us("since_us"),
            boot_us: us("boot_us"),
            packages_us: us("packages_us"),
        }
    }

    fn rows(&self) -> Value {
        let us = |v: Option<u64>| v.map(|v| v.to_string());
        json!({
            "cursor": self.cursor,
            "since_us": us(self.since_us),
            "boot_us": us(self.boot_us),
            "packages_us": us(self.packages_us),
        })
    }
}

/// The record the database writer turns into `events` rows and the resume point; the JSONL log never sees it.
fn record(events: &[Event], state: &State, error: Option<&str>) -> Value {
    let mut r = json!({"type": "events", "t": unix_now(), "events": events, "state": state.rows()});
    if let Some(e) = error {
        r["errors"] = json!({ "events": e });
    }
    r
}

trait Source {
    fn seek(&mut self, to: Seek) -> Result<()>;
    fn next(&mut self) -> Result<Option<Entry>>;
    fn boots(&mut self) -> Result<Vec<Boot>>;
    /// Whether `boot` logged a shutdown or the journal stopping. The read position is lost.
    fn ended_cleanly(&mut self, boot: &str) -> Result<bool>;
}

/// Entries with one of the ids, and kernel errors, where the OOM killer's lines are: (ids or kernel) and (ids or
/// priority 0-3).
fn stream_filter() -> Filter {
    let by_id = || STREAM_IDS.iter().map(|id| ("MESSAGE_ID", *id));
    Filter::default()
        .any(by_id().chain([("_TRANSPORT", "kernel")]))
        .any(by_id().chain(["0", "1", "2", "3"].map(|p| ("PRIORITY", p))))
}

struct Live {
    j: Journal,
    fields: Vec<&'static str>,
}

impl Live {
    fn open() -> Result<Live> {
        let mut j = Journal::open()?;
        j.set_filter(&stream_filter())?;
        let mut fields = INDEX_FIELDS.to_vec();
        fields.push("__CURSOR");
        Ok(Live { j, fields })
    }
}

impl Source for Live {
    fn seek(&mut self, to: Seek) -> Result<()> {
        self.j.seek(to)
    }

    fn next(&mut self) -> Result<Option<Entry>> {
        self.j.next(Fields::Only(&self.fields))
    }

    fn boots(&mut self) -> Result<Vec<Boot>> {
        self.j.boots()
    }

    fn ended_cleanly(&mut self, boot: &str) -> Result<bool> {
        let stops = Filter::default()
            .any([("_BOOT_ID", boot)])
            .any([("MESSAGE_ID", ids::SHUTDOWN), ("MESSAGE_ID", ids::JOURNAL_STOPPED)]);
        self.j.set_filter(&stops)?;
        let found = self
            .j
            .seek(Seek::Head)
            .and_then(|_| self.j.next(Fields::Only(&["MESSAGE_ID"])));
        self.j.set_filter(&stream_filter())?;
        Ok(found?.is_some())
    }
}

struct Recorder {
    detector: Detector,
    state: State,
    index: usize,
    /// The cursor seeked to; its entry was read before the restart.
    skip: Option<String>,
}

impl Recorder {
    fn new(state: State) -> Recorder {
        Recorder {
            detector: Detector::new(),
            state,
            index: 0,
            skip: None,
        }
    }

    /// Seeks to where the last run stopped, or to `from_us` on the first run.
    fn start(&mut self, src: &mut impl Source, from_us: u64) -> Result<()> {
        match (self.state.since_us, &self.state.cursor) {
            (Some(since), _) => src.seek(Seek::Realtime(since)),
            (None, Some(cursor)) => {
                self.skip = Some(cursor.clone());
                src.seek(Seek::Cursor(cursor.clone()))
            }
            (None, None) => src.seek(Seek::Realtime(from_us)),
        }
    }

    /// Goes on from just after the last entry read, on a new handle; the rules' state carries over.
    fn continue_on(&mut self, src: &mut impl Source) -> Result<()> {
        match self.state.cursor.clone() {
            Some(cursor) => {
                self.skip = Some(cursor.clone());
                src.seek(Seek::Cursor(cursor))
            }
            None => src.seek(Seek::Tail),
        }
    }

    /// Reads up to `CHUNK` entries and returns their events, and whether the end of the journal was reached. `now_us`
    /// closes rule windows at the end; within a long read the last entry's time does.
    fn read(&mut self, src: &mut impl Source, now_us: u64) -> Result<(Vec<Event>, bool)> {
        let mut out = Vec::new();
        let mut last_us = None;
        for _ in 0..CHUNK {
            let Some(e) = src.next()? else {
                self.drain(now_us, &mut out);
                return Ok((out, true));
            };
            let cursor = e.get("__CURSOR").map(str::to_string);
            if self.skip.take().is_some_and(|c| cursor.as_ref() == Some(&c)) {
                continue;
            }
            out.extend(sleep_event(&e));
            self.detector.push(self.index, &e);
            self.index += 1;
            last_us = Some(e.t_us);
            if cursor.is_some() {
                self.state.cursor = cursor;
            }
        }
        self.drain(last_us.unwrap_or(now_us), &mut out);
        Ok((out, false))
    }

    fn drain(&mut self, now_us: u64, out: &mut Vec<Event>) {
        out.extend(self.detector.drain(now_us).iter().filter_map(finding_event));
        self.state.since_us = self.detector.open_since();
    }
}

fn secs(us: u64) -> i64 {
    (us / 1_000_000) as i64
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

fn finding_event(f: &Finding) -> Option<Event> {
    let field = |k: &str| f.data.get(k).map(String::as_str);
    let (kind, summary) = match f.kind {
        // the detector numbers repeat crashes from where it started reading; the row must not depend on that
        FindingKind::Crash => (
            "crash",
            format!(
                "{} crashed ({})",
                field("comm").unwrap_or("A process"),
                field("signal").unwrap_or("unknown signal")
            ),
        ),
        FindingKind::OomKill => ("oom", f.title.clone()),
        FindingKind::FailedUnit => ("unit_failed", f.title.clone()),
        FindingKind::RestartLoop => ("restart_loop", f.title.clone()),
        _ => return None,
    };
    let mut data: Map<String, Value> = f
        .data
        .iter()
        .map(|(k, v)| {
            let v = match v.parse::<i64>() {
                Ok(n) if k == "pid" => n.into(),
                _ => v.as_str().into(),
            };
            (k.clone(), v)
        })
        .collect();
    match f.kind {
        FindingKind::FailedUnit => data.insert("count".into(), f.count.into()),
        FindingKind::RestartLoop => data.insert("n_restarts".into(), f.count.into()),
        _ => None,
    };
    Some(Event {
        t: secs(f.first_us),
        kind: kind.into(),
        key: f.cursor.clone().unwrap_or_else(|| f.first_us.to_string()),
        boot_id: Some(f.boot_id.clone()).filter(|b| !b.is_empty()),
        summary,
        data: data.into(),
    })
}

fn sleep_event(e: &Entry) -> Option<Event> {
    let (kind, summary) = match e.get("MESSAGE_ID")? {
        ids::SLEEP_START => ("suspend", "Suspended"),
        ids::SLEEP_STOP => ("resume", "Resumed"),
        _ => return None,
    };
    Some(Event {
        t: secs(e.t_us),
        kind: kind.into(),
        key: e.get("__CURSOR").map_or_else(|| e.t_us.to_string(), str::to_string),
        boot_id: e.get("_BOOT_ID").map(str::to_string),
        summary: summary.into(),
        data: json!({}),
    })
}

/// A `boot` for each boot that started after the newest one recorded, and a `boot_end` for the boot before it, which
/// has ended by then. Without the system journal there is no telling how a boot ended, so `full` false leaves
/// `boot_end` out.
fn boot_events(src: &mut impl Source, state: &mut State, from_us: u64, full: bool) -> Result<Vec<Event>> {
    let boots = src.boots()?;
    let mut out = Vec::new();
    for (i, b) in boots.iter().enumerate() {
        if b.first_us < from_us || state.boot_us.is_some_and(|done| b.first_us <= done) {
            continue;
        }
        out.push(Event {
            t: secs(b.first_us),
            kind: "boot".into(),
            key: b.id.clone(),
            boot_id: Some(b.id.clone()),
            summary: "Booted".into(),
            data: json!({}),
        });
        if let Some(prev) = i.checked_sub(1).map(|p| &boots[p])
            && full
        {
            let clean = src.ended_cleanly(&prev.id)?;
            out.push(Event {
                t: secs(prev.last_us),
                kind: "boot_end".into(),
                key: prev.id.clone(),
                boot_id: Some(prev.id.clone()),
                summary: if clean {
                    "Shut down"
                } else {
                    "Boot ended without a shutdown"
                }
                .into(),
                data: json!({ "clean": clean }),
            });
        }
        state.boot_us = Some(b.first_us);
    }
    Ok(out)
}

struct Packages {
    dir: PathBuf,
    seen: Option<Vec<Option<SystemTime>>>,
}

impl Packages {
    fn new(dir: &Path) -> Packages {
        Packages {
            dir: dir.to_path_buf(),
            seen: None,
        }
    }

    /// One event per transaction since the newest recorded, read only when a log's mtime changed. The newest is
    /// read again, as it may still have been running.
    fn poll(&mut self, state: &mut State, from_us: u64) -> Vec<Event> {
        let stamp: Vec<_> = PACKAGE_LOGS
            .iter()
            .map(|f| std::fs::metadata(self.dir.join(f)).and_then(|m| m.modified()).ok())
            .collect();
        if self.seen.as_ref() == Some(&stamp) {
            return Vec::new();
        }
        self.seen = Some(stamp);
        let since = state.packages_us.unwrap_or(from_us);
        let txs = packages::read_history_in(&self.dir, since, &TimeZone::system());
        if let Some(newest) = txs.iter().map(|t| t.start_us).max() {
            state.packages_us = Some(newest.max(since));
        }
        txs.iter().map(package_event).collect()
    }
}

fn package_event(tx: &Transaction) -> Event {
    let word = |a| format!("{a:?}").to_lowercase();
    let mut counts: Vec<(String, usize)> = Vec::new();
    for c in &tx.changes {
        let w = word(c.action);
        match counts.iter_mut().find(|(k, _)| *k == w) {
            Some((_, n)) => *n += 1,
            None => counts.push((w, 1)),
        }
    }
    counts.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    let summary = counts
        .iter()
        .enumerate()
        .map(|(i, (w, n))| match (i, n) {
            (0, 1) => format!("1 package {w}"),
            (0, n) => format!("{n} packages {w}"),
            (_, n) => format!("{n} {w}"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let changes: Vec<Value> = tx
        .changes
        .iter()
        .map(|c| json!({"action": word(c.action), "name": c.name, "old": c.old, "new": c.new}))
        .collect();
    Event {
        t: secs(tx.start_us),
        kind: "packages".into(),
        key: tx.start_us.to_string(),
        boot_id: None,
        summary,
        data: json!({
            "manager": format!("{:?}", tx.manager).to_lowercase(),
            "command": tx.command,
            "requested_by": tx.requested_by,
            "changes": changes,
        }),
    }
}

struct Out {
    tx: SyncSender<Value>,
    /// What the database was last sent; a retry resumes from it.
    saved: State,
    said_own_only: bool,
}

impl Out {
    /// Waits for room in the writer's queue: this thread can fall behind, the scheduler cannot.
    fn send(&mut self, events: Vec<Event>, state: &State, error: Option<&str>) -> Result<()> {
        if events.is_empty() && *state == self.saved && error.is_none() {
            return Ok(());
        }
        self.tx
            .send(record(&events, state, error))
            .map_err(|_| anyhow!("the database writer has stopped"))?;
        self.saved = state.clone();
        Ok(())
    }
}

pub fn spawn(tx: SyncSender<Value>, state: State, events_days: u32) {
    thread::Builder::new()
        .name("events".into())
        .spawn(move || serve(tx, state, events_days))
        .expect("spawning the events thread");
}

fn serve(tx: SyncSender<Value>, state: State, events_days: u32) {
    let mut out = Out {
        tx,
        saved: state,
        said_own_only: false,
    };
    let mut backoff = Duration::from_secs(1);
    let mut last_error = None;
    loop {
        let started = Instant::now();
        let Err(e) = follow(&mut out, events_days, Path::new("/var/log"));
        let msg = format!("{e:#}");
        if last_error.as_ref() != Some(&msg) {
            eprintln!("events: {msg}");
            last_error = Some(msg.clone());
        }
        let saved = out.saved.clone();
        if out.send(Vec::new(), &saved, Some(&msg)).is_err() {
            eprintln!("events: the database writer has stopped; events are not being recorded");
            return;
        }
        if started.elapsed() > BACKOFF_MAX {
            backoff = Duration::from_secs(1);
        }
        thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

fn follow(out: &mut Out, events_days: u32, log_dir: &Path) -> Result<Infallible> {
    let access = reader::access();
    if access == Access::OwnOnly && !out.said_own_only {
        let saved = out.saved.clone();
        out.send(
            Vec::new(),
            &saved,
            Some("only this user's journal is readable: system crashes and failures and how boots ended are missing"),
        )?;
        out.said_own_only = true;
    }
    let mut src = Live::open()?;
    let from_us = now_us().saturating_sub(u64::from(events_days) * DAY_US);
    let mut rec = Recorder::new(out.saved.clone());
    let boots = boot_events(&mut src, &mut rec.state, from_us, access == Access::Full)?;
    out.send(boots, &rec.state, None)?;
    rec.start(&mut src, from_us)?;
    let mut packages = Packages::new(log_dir);
    let mut packages_due = Instant::now();
    let mut caught_up = false;
    loop {
        let (mut events, end) = rec.read(&mut src, now_us().saturating_sub(LATE_US))?;
        if Instant::now() >= packages_due {
            events.extend(packages.poll(&mut rec.state, from_us));
            packages_due = Instant::now() + PACKAGES_EVERY;
        }
        out.send(events, &rec.state, None)?;
        if end {
            if !caught_up {
                // listing boots and catching up mapped every journal file, hundreds of MB; a new handle maps only
                // what following needs
                src = Live::open()?;
                rec.continue_on(&mut src)?;
            }
            caught_up = true;
            src.j.wait(POLL)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};

    use super::*;
    use crate::config::Retention;
    use crate::db::Db;

    const BOOT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const S: u64 = 1_000_000;

    #[derive(Default)]
    struct Fake {
        entries: Vec<Entry>,
        at: usize,
        read: usize,
        boots: Vec<Boot>,
        clean: Vec<String>,
    }

    impl Fake {
        fn add(&mut self, t_s: u64, fields: &[(&str, &str)]) {
            let mut map: BTreeMap<String, String> =
                fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            map.insert("_BOOT_ID".into(), BOOT.into());
            map.insert("__CURSOR".into(), format!("c{}", self.entries.len()));
            self.entries.push(Entry {
                t_us: t_s * S,
                fields: map,
            });
        }

        fn crash(&mut self, t_s: u64, comm: &str) {
            self.add(
                t_s,
                &[
                    ("MESSAGE_ID", ids::COREDUMP),
                    ("COREDUMP_COMM", comm),
                    ("COREDUMP_PID", "4242"),
                    ("COREDUMP_SIGNAL_NAME", "SIGSEGV"),
                ],
            );
        }

        fn restart(&mut self, t_s: u64) {
            self.add(t_s, &[("MESSAGE_ID", ids::RESTART_SCHEDULED), ("UNIT", "x.service")]);
        }
    }

    impl Source for Fake {
        fn seek(&mut self, to: Seek) -> Result<()> {
            let at = |p: Option<usize>| p.unwrap_or(self.entries.len());
            self.at = match to {
                Seek::Head => 0,
                Seek::Tail => self.entries.len(),
                Seek::Realtime(t) => at(self.entries.iter().position(|e| e.t_us >= t)),
                Seek::Cursor(c) => at(self.entries.iter().position(|e| e.get("__CURSOR") == Some(&c))),
            };
            Ok(())
        }

        fn next(&mut self) -> Result<Option<Entry>> {
            let e = self.entries.get(self.at).cloned();
            if e.is_some() {
                self.at += 1;
                self.read += 1;
            }
            Ok(e)
        }

        fn boots(&mut self) -> Result<Vec<Boot>> {
            Ok(self.boots.clone())
        }

        fn ended_cleanly(&mut self, boot: &str) -> Result<bool> {
            Ok(self.clean.iter().any(|b| b == boot))
        }
    }

    fn open(dir: &tempfile::TempDir) -> Db {
        // the fixtures' times are near 1970; a 90-day window would prune them on the first write
        let keep = Retention {
            events_days: 100_000,
            ..Retention::default()
        };
        Db::open(&dir.path().join("h.db"), keep, HashSet::new()).unwrap()
    }

    /// One run of the thread's read loop up to the end of the journal, resuming from what the database holds.
    fn run(db: &mut Db, src: &mut Fake, now_s: u64) {
        let mut rec = Recorder::new(State::from_rows(&db.events_state().unwrap()));
        src.read = 0;
        rec.start(src, 0).unwrap();
        loop {
            let (events, end) = rec.read(src, now_s * S).unwrap();
            db.write(&record(&events, &rec.state, None)).unwrap();
            if end {
                break;
            }
        }
    }

    fn rows(db: &Db) -> Vec<(i64, String, String, String, String)> {
        db.conn()
            .prepare("SELECT t, kind, key, summary, data FROM events ORDER BY t, kind, key")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn rereading_the_journal_writes_no_duplicates() {
        let mut src = Fake::default();
        src.crash(100, "Hyprland");
        for t in [200, 210, 220, 230] {
            src.restart(t);
        }
        src.add(
            300,
            &[
                ("MESSAGE_ID", ids::UNIT_FAILURE),
                ("UNIT", "backup.service"),
                ("UNIT_RESULT", "exit-code"),
            ],
        );
        src.add(400, &[("MESSAGE_ID", ids::SLEEP_START)]);
        src.add(500, &[("MESSAGE_ID", ids::SLEEP_STOP)]);
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        run(&mut db, &mut src, 10_000);
        let first = rows(&db);
        let kinds: Vec<_> = first.iter().map(|r| r.1.as_str()).collect();
        assert_eq!(kinds, ["crash", "restart_loop", "unit_failed", "suspend", "resume"]);
        assert_eq!(first[0].3, "Hyprland crashed (SIGSEGV)");
        assert_eq!(first[1].2, "c1", "a loop is keyed by its first restart");

        // a lost resume point means reading everything again; the keys must make that harmless
        db.write(&json!({"type": "events", "t": 1.0, "events": [], "state": State::default().rows()}))
            .unwrap();
        run(&mut db, &mut src, 10_000);
        assert_eq!(rows(&db), first);
    }

    #[test]
    fn a_restart_resumes_where_it_stopped_without_splitting_a_loop() {
        let mut src = Fake::default();
        src.crash(100, "waybar");
        src.restart(1000);
        src.restart(1060);
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        run(&mut db, &mut src, 1060);
        assert_eq!(rows(&db).len(), 1, "two restarts are not a loop yet");

        // the daemon stops here; the unit keeps restarting
        src.restart(1120);
        run(&mut db, &mut src, 1120);
        assert_eq!(src.read, 3, "read again from the open restarts, not from the start");
        assert_eq!(rows(&db)[1].1, "restart_loop", "the loop formed across the restart");

        src.restart(1180);
        src.crash(2000, "waybar");
        run(&mut db, &mut src, 3000);
        let rows = rows(&db);
        let kinds: Vec<_> = rows.iter().map(|r| r.1.as_str()).collect();
        assert_eq!(kinds, ["crash", "restart_loop", "crash"], "one loop, not one per read");
        let data: Value = serde_json::from_str(&rows[1].4).unwrap();
        assert_eq!(data["n_restarts"], 4, "the row grows with the loop");
        assert_eq!((rows[1].0, rows[1].2.as_str()), (1000, "c1"));

        run(&mut db, &mut src, 4000);
        assert_eq!(
            src.read, 1,
            "nothing open: only the last entry is seen again, and skipped"
        );
        assert_eq!(self::rows(&db).len(), 3);
    }

    #[test]
    fn events_older_than_events_days_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(
            &dir.path().join("h.db"),
            Retention {
                events_days: 30,
                ..Retention::default()
            },
            HashSet::new(),
        )
        .unwrap();
        let now = unix_now() as i64;
        let event = |t: i64, key: &str| Event {
            t,
            kind: "crash".into(),
            key: key.into(),
            boot_id: None,
            summary: "x".into(),
            data: json!({}),
        };
        let events = [event(now - 31 * 86400, "old"), event(now - 29 * 86400, "kept")];
        db.write(&record(&events, &State::default(), None)).unwrap();
        let keys: Vec<_> = rows(&db).into_iter().map(|r| r.2).collect();
        assert_eq!(keys, ["kept"]);
    }

    fn boot(id: &str, first_s: u64, last_s: u64) -> Boot {
        Boot {
            id: id.into(),
            first_us: first_s * S,
            last_us: last_s * S,
        }
    }

    #[test]
    fn a_boot_that_shut_down_ends_clean_and_one_that_lost_power_does_not() {
        let mut src = Fake {
            boots: vec![boot("a", 100, 200), boot("b", 300, 400), boot("c", 500, 600)],
            clean: vec!["a".into()],
            ..Fake::default()
        };
        let mut state = State::default();
        let events = boot_events(&mut src, &mut state, 0, true).unwrap();
        let got: Vec<_> = events
            .iter()
            .map(|e| (e.kind.as_str(), e.key.as_str(), e.t, e.data.get("clean").cloned()))
            .collect();
        assert_eq!(
            got,
            [
                ("boot", "a", 100, None),
                ("boot", "b", 300, None),
                ("boot_end", "a", 200, Some(true.into())),
                ("boot", "c", 500, None),
                ("boot_end", "b", 400, Some(false.into())),
            ],
            "the running boot has not ended"
        );
        assert!(boot_events(&mut src, &mut state, 0, true).unwrap().is_empty());

        src.boots.push(boot("d", 700, 800));
        let next = boot_events(&mut src, &mut state, 0, true).unwrap();
        let got: Vec<_> = next.iter().map(|e| (e.kind.as_str(), e.key.as_str())).collect();
        assert_eq!(
            got,
            [("boot", "d"), ("boot_end", "c")],
            "decided once the next boot runs"
        );

        let mut own = State::default();
        let events = boot_events(&mut src, &mut own, 0, false).unwrap();
        assert!(
            events.iter().all(|e| e.kind == "boot"),
            "without the system journal every boot would look abrupt"
        );
    }

    const PACMAN: &str = "\
[2026-03-01T10:00:00+0000] [PACMAN] Running 'pacman -S foo'
[2026-03-01T10:00:02+0000] [ALPM] transaction started
[2026-03-01T10:00:02+0000] [ALPM] installed foo (1.0-1)
[2026-03-01T10:00:03+0000] [ALPM] transaction completed
[2026-03-02T11:00:00+0000] [PACMAN] Running 'pacman -Syu'
[2026-03-02T11:00:05+0000] [ALPM] transaction started
[2026-03-02T11:00:07+0000] [ALPM] upgraded bar (2.0-1 -> 2.1-1)
[2026-03-02T11:00:07+0000] [ALPM] upgraded baz (1-1 -> 2-1)
[2026-03-02T11:00:08+0000] [ALPM] removed qux (3.0-1)
[2026-03-02T11:00:10+0000] [ALPM] transaction completed
";

    #[test]
    fn each_package_transaction_is_one_event_written_once() {
        let logs = tempfile::tempdir().unwrap();
        let log = logs.path().join("pacman.log");
        std::fs::write(&log, PACMAN).unwrap();
        let mut packages = Packages::new(logs.path());
        let mut state = State::default();
        let events = packages.poll(&mut state, 0);
        let summaries: Vec<_> = events.iter().map(|e| e.summary.as_str()).collect();
        assert_eq!(summaries, ["1 package installed", "2 packages upgraded, 1 removed"]);
        assert_eq!(events[1].data["command"], "pacman -Syu");
        assert_eq!(events[1].data["changes"][0]["old"], "2.0-1");
        assert!(packages.poll(&mut state, 0).is_empty(), "the logs did not change");

        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        db.write(&record(&events, &state, None)).unwrap();
        let mut more = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        std::io::Write::write_all(
            &mut more,
            b"[2026-03-03T09:00:01+0000] [ALPM] transaction started\n[2026-03-03T09:00:02+0000] [ALPM] removed gone (1-1)\n",
        )
        .unwrap();
        more.set_modified(SystemTime::now() + Duration::from_secs(5)).unwrap();
        let events = packages.poll(&mut state, 0);
        db.write(&record(&events, &state, None)).unwrap();
        assert_eq!(rows(&db).len(), 3, "the newest is read again, never written twice");
    }
}
