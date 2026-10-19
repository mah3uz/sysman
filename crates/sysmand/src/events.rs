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
use journal::findings::{
    BootEndFacts, BootEndVerdict, Detector, Finding, FindingKind, ResetKind, boot_end_verdict, decode_reset_reason,
};
use journal::packages::{self, Transaction};
use journal::reader::{self, Access, Boot, Change, Fields, Filter, Journal, Seek};
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
const JOURNAL_DIRS: [&str; 2] = ["/var/log/journal", "/run/log/journal"];
const ROTATED_MARGIN: Duration = Duration::from_secs(600);
const FILES_EVERY: Duration = Duration::from_secs(60);
const KERNEL_HEAD: usize = 8;
const JOURNALD_WARNINGS: usize = 200;
const KERNEL_BOOT_LINES: usize = 5000;
// The AMD reset reason is printed during CPU setup, in the boot's first seconds.
const RESET_REASON_US: u64 = 120_000_000;

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
    fn set_filter(&mut self, filter: &Filter) -> Result<()>;
    fn seek(&mut self, to: Seek) -> Result<()>;
    fn next(&mut self) -> Result<Option<Entry>>;
    fn boots(&mut self) -> Result<Vec<Boot>>;
}

/// Entries with one of the ids, and kernel lines but debug: (ids or kernel) and (ids or priority 0-6). Xid lines
/// are warnings, GPU hangs info or notice and segfaults info.
fn stream_filter() -> Filter {
    let by_id = || STREAM_IDS.iter().map(|id| ("MESSAGE_ID", *id));
    Filter::default()
        .any(by_id().chain([("_TRANSPORT", "kernel")]))
        .any(by_id().chain(["0", "1", "2", "3", "4", "5", "6"].map(|p| ("PRIORITY", p))))
}

struct Live {
    j: Journal,
    fields: Vec<&'static str>,
    /// The files opened, or none for every file.
    files: Vec<PathBuf>,
}

impl Live {
    fn open() -> Result<Live> {
        Live::with(Journal::open()?, Vec::new())
    }

    /// Only the files that can hold entries after `after_us`. A handle on every file maps each one it touches, and
    /// a filtered step touches them all: hundreds of MB of RSS. Without libsystemd, `journalctl` maps nothing here.
    fn open_growing(after_us: u64) -> Result<Live> {
        let files = growing_files(&journal_dirs(), after_us);
        match Journal::open_files(&files) {
            Ok(j) if !files.is_empty() => Live::with(j, files),
            _ => Live::open(),
        }
    }

    fn with(mut j: Journal, files: Vec<PathBuf>) -> Result<Live> {
        j.set_filter(&stream_filter())?;
        let mut fields = INDEX_FIELDS.to_vec();
        fields.push("__CURSOR");
        Ok(Live { j, fields, files })
    }
}

fn journal_dirs() -> Vec<PathBuf> {
    let Ok(id) = std::fs::read_to_string("/etc/machine-id") else {
        return Vec::new();
    };
    JOURNAL_DIRS.iter().map(|d| Path::new(d).join(id.trim())).collect()
}

fn journal_files(dirs: &[PathBuf]) -> Vec<(PathBuf, std::fs::Metadata)> {
    dirs.iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".journal") || name.ends_with(".journal~")
        })
        .filter_map(|e| Some((e.path(), e.metadata().ok()?)))
        .collect()
}

/// The files written since shortly before `after_us` and readable: the active ones, and any rotated or renamed
/// since, whose tail may not have been read yet.
fn growing_files(dirs: &[PathBuf], after_us: u64) -> Vec<PathBuf> {
    let after = UNIX_EPOCH + Duration::from_micros(after_us) - ROTATED_MARGIN.min(Duration::from_micros(after_us));
    let mut files: Vec<_> = journal_files(dirs)
        .into_iter()
        .filter(|(_, m)| m.modified().is_ok_and(|t| t >= after))
        .filter(|(p, _)| std::fs::File::open(p).is_ok())
        .map(|(p, _)| p)
        .collect();
    files.sort();
    files
}

/// The files journald writes to now, by inode; a change is a rotation or a new user's journal.
fn active_files(dirs: &[PathBuf]) -> Vec<(PathBuf, u64)> {
    let mut files: Vec<_> = journal_files(dirs)
        .into_iter()
        .filter(|(p, _)| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            name.ends_with(".journal") && !name.contains('@')
        })
        .map(|(p, m)| (p, std::os::unix::fs::MetadataExt::ino(&m)))
        .collect();
    files.sort();
    files
}

fn rotated(active: &[(PathBuf, u64)]) -> bool {
    active.iter().any(|(p, ino)| {
        std::fs::metadata(p)
            .map(|m| std::os::unix::fs::MetadataExt::ino(&m))
            .ok()
            != Some(*ino)
    })
}

impl Source for Live {
    fn set_filter(&mut self, filter: &Filter) -> Result<()> {
        self.j.set_filter(filter)
    }

    fn seek(&mut self, to: Seek) -> Result<()> {
        self.j.seek(to)
    }

    fn next(&mut self) -> Result<Option<Entry>> {
        self.j.next(Fields::Only(&self.fields))
    }

    fn boots(&mut self) -> Result<Vec<Boot>> {
        self.j.boots()
    }
}

/// What `boot`'s markers say about how it ended and, when they leave it abrupt, what the start of `next` says:
/// journald finding a file left open, and the AMD reset reason. Filtered reads only, never a whole boot. The stream
/// filter is set again after; the read position is lost.
fn boot_end_facts(src: &mut impl Source, boot: &Boot, next: &Boot) -> Result<BootEndFacts> {
    let facts = read_boot_end(src, boot, next);
    src.set_filter(&stream_filter())?;
    facts
}

fn read_boot_end(src: &mut impl Source, boot: &Boot, next: &Boot) -> Result<BootEndFacts> {
    let mut f = BootEndFacts {
        last_us: boot.last_us,
        ..BootEndFacts::default()
    };
    let kernel = |id: &str| Filter::default().any([("_BOOT_ID", id)]).any([("_TRANSPORT", "kernel")]);
    feed(src, &kernel(&boot.id), KERNEL_HEAD, |e| {
        f.note(e);
        !f.kernel_start
    })?;
    if !f.kernel_start {
        return Ok(f);
    }
    let markers = || {
        [ids::SHUTDOWN, ids::JOURNAL_STOPPED, ids::SLEEP_START, ids::SLEEP_STOP]
            .map(|id| ("MESSAGE_ID", id))
            .into_iter()
    };
    // (a marker or multi-user.target) and (a marker or a unit stopped): that target's stop, not every unit's
    let own = Filter::default()
        .any([("_BOOT_ID", boot.id.as_str())])
        .any(markers().chain([("UNIT", "multi-user.target")]))
        .any(markers().chain([("MESSAGE_ID", ids::UNIT_STOPPED)]));
    feed(src, &own, usize::MAX, |e| {
        f.note(e);
        true
    })?;
    if boot_end_verdict(&f) == BootEndVerdict::Clean {
        return Ok(f);
    }
    let journald = Filter::default()
        .any([("_BOOT_ID", next.id.as_str())])
        .any([("SYSLOG_IDENTIFIER", "systemd-journald")])
        .any(["0", "1", "2", "3", "4"].map(|p| ("PRIORITY", p)));
    feed(src, &journald, JOURNALD_WARNINGS, |e| {
        f.note_next(e);
        !f.next_unclean_journal
    })?;
    feed(src, &kernel(&next.id), KERNEL_BOOT_LINES, |e| {
        f.note_next(e);
        f.next_reset_reason.is_none() && e.t_us <= next.first_us + RESET_REASON_US
    })?;
    Ok(f)
}

/// Gives up to `max` entries passing `filter`, oldest first, to `each` until it returns false.
fn feed(src: &mut impl Source, filter: &Filter, max: usize, mut each: impl FnMut(&Entry) -> bool) -> Result<()> {
    src.set_filter(filter)?;
    src.seek(Seek::Head)?;
    for _ in 0..max {
        match src.next()? {
            Some(e) if each(&e) => {}
            _ => break,
        }
    }
    Ok(())
}

struct Recorder {
    detector: Detector,
    state: State,
    index: usize,
    /// The newest entry's time read so far.
    read_us: u64,
    /// The cursor seeked to; its entry was read before the restart.
    skip: Option<String>,
}

impl Recorder {
    /// Believes unit failures and restarts only from PID 1 and the user manager of `owner_uid`.
    fn new(state: State, owner_uid: u32) -> Recorder {
        Recorder {
            detector: Detector::for_owner(owner_uid),
            state,
            index: 0,
            read_us: 0,
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
            self.read_us = self.read_us.max(e.t_us);
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
    let comm = field("comm").unwrap_or("A process");
    let (kind, summary) = match f.kind {
        // the detector numbers repeat crashes from where it started reading; the row must not depend on that
        FindingKind::Crash => ("crash", crash_summary(comm, field("signal").unwrap_or("unknown signal"), 1)),
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
    let mut key = f.cursor.clone().unwrap_or_else(|| f.first_us.to_string());
    match f.kind {
        FindingKind::FailedUnit => data.insert("count".into(), f.count.into()),
        FindingKind::RestartLoop => data.insert("n_restarts".into(), f.count.into()),
        // one row per program per boot, which the database folds into; see `fold_crash`
        FindingKind::Crash => {
            key = comm.to_string();
            data.insert("last_us".into(), f.first_us.into());
            data.insert("count".into(), 1.into())
        }
        _ => None,
    };
    Some(Event {
        t: secs(f.first_us),
        kind: kind.into(),
        key,
        boot_id: Some(f.boot_id.clone()).filter(|b| !b.is_empty()),
        summary,
        data: data.into(),
    })
}

pub fn crash_summary(comm: &str, signal: &str, count: u64) -> String {
    match count {
        0 | 1 => format!("{comm} crashed ({signal})"),
        n => format!("{comm} crashed {n} times ({signal})"),
    }
}

fn sleep_event(e: &Entry) -> Option<Event> {
    // systemd-sleep runs as root; any user can log the same MESSAGE_ID
    if e.get("_UID") != Some("0") {
        return None;
    }
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
/// `boot_end` out; so does a boot whose start was vacuumed, which reads as unknown, as a missing row does.
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
            out.extend(boot_end_event(prev, &boot_end_facts(src, prev, b)?));
        }
        state.boot_us = Some(b.first_us);
    }
    Ok(out)
}

fn boot_end_event(boot: &Boot, facts: &BootEndFacts) -> Option<Event> {
    let verdict = boot_end_verdict(facts);
    let reason = facts.next_reset_reason.as_deref().map(decode_reset_reason);
    let suspended = facts.ended_suspended();
    // only a hardware power cut is called a power loss; an orderly reason does not explain an abrupt end
    let summary = match (verdict, reason) {
        (BootEndVerdict::Unknown, _) => return None,
        (BootEndVerdict::Clean, _) => "Shut down".to_string(),
        (_, Some((label, ResetKind::PowerCut))) => format!("Power lost: {label}"),
        _ if suspended => "Ended while suspended".to_string(),
        (_, Some((label, ResetKind::Abrupt))) => format!("Ended without shutdown ({label})"),
        _ => "Ended without shutdown".to_string(),
    };
    Some(Event {
        t: secs(boot.last_us),
        kind: "boot_end".into(),
        key: boot.id.clone(),
        boot_id: Some(boot.id.clone()),
        summary,
        data: json!({
            "clean": verdict == BootEndVerdict::Clean,
            "certainty": verdict.key(),
            "reset_reason": reason.map(|(label, _)| label),
            "suspended": suspended,
        }),
    })
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
    // SAFETY: geteuid has no preconditions and cannot fail.
    let mut rec = Recorder::new(out.saved.clone(), unsafe { libc::geteuid() });
    // without boot records the other events still matter; a later start tries the boots again
    let mut state = rec.state.clone();
    match boot_events(&mut src, &mut state, from_us, access == Access::Full) {
        Ok(boots) => {
            rec.state = state;
            out.send(boots, &rec.state, None)?;
        }
        Err(e) => {
            let msg = format!("reading boots: {e:#}");
            eprintln!("events: {msg}");
            out.send(Vec::new(), &rec.state, Some(&msg))?;
            src.set_filter(&stream_filter())?;
        }
    }
    rec.start(&mut src, from_us)?;
    let mut packages = Packages::new(log_dir);
    let mut packages_due = Instant::now();
    // Set once caught up and following on a handle restricted to the files that can still grow.
    let mut following: Option<Vec<(PathBuf, u64)>> = None;
    let dirs = journal_dirs();
    let mut files_checked = Instant::now();
    let mut change = Change::Nothing;
    loop {
        if let Some(active) = &following {
            // a rotated file leaves the handle, so reopen before reading on, or its unread tail would be skipped; an
            // archive read to its end is let go
            let full_check = change == Change::FilesChanged || files_checked.elapsed() >= FILES_EVERY;
            let changed = rotated(active)
                || (full_check
                    && (*active != active_files(&dirs)
                        || (!src.files.is_empty() && src.files != growing_files(&dirs, rec.read_us))));
            if full_check {
                files_checked = Instant::now();
            }
            if changed {
                following = Some(active_files(&dirs));
                src = Live::open_growing(rec.read_us)?;
                rec.continue_on(&mut src)?;
            }
        }
        let (mut events, end) = rec.read(&mut src, now_us().saturating_sub(LATE_US))?;
        if Instant::now() >= packages_due {
            events.extend(packages.poll(&mut rec.state, from_us));
            packages_due = Instant::now() + PACKAGES_EVERY;
        }
        out.send(events, &rec.state, None)?;
        change = Change::Nothing;
        if end {
            if following.is_none() {
                following = Some(active_files(&dirs));
                src = Live::open_growing(rec.read_us)?;
                rec.continue_on(&mut src)?;
                // the catch-up's freed allocations stay resident in this thread's and the writer's arenas otherwise
                #[cfg(target_env = "gnu")]
                // SAFETY: malloc_trim only returns free heap pages to the kernel.
                unsafe {
                    libc::malloc_trim(0);
                }
            }
            change = src.j.wait(POLL)?;
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
        filter: Filter,
    }

    const OWNER: u32 = 1000;
    const PID1: [(&str, &str); 3] = [("_PID", "1"), ("_UID", "0"), ("_COMM", "systemd")];

    impl Fake {
        fn add(&mut self, t_s: u64, fields: &[(&str, &str)]) {
            self.add_in(BOOT, t_s, fields);
        }

        fn add_in(&mut self, boot: &str, t_s: u64, fields: &[(&str, &str)]) {
            let mut map: BTreeMap<String, String> =
                fields.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            map.insert("_BOOT_ID".into(), boot.into());
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
            self.add(
                t_s,
                &[[("MESSAGE_ID", ids::RESTART_SCHEDULED), ("UNIT", "x.service")].as_slice(), &PID1].concat(),
            );
        }

        fn kernel(&mut self, boot: &str, t_s: u64, priority: &str, message: &str) {
            self.add_in(
                boot,
                t_s,
                &[("_TRANSPORT", "kernel"), ("PRIORITY", priority), ("MESSAGE", message)],
            );
        }
    }

    impl Source for Fake {
        fn set_filter(&mut self, filter: &Filter) -> Result<()> {
            self.filter = filter.clone();
            Ok(())
        }

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
            let passes = |e: &Entry| {
                self.filter.0.iter().all(|group| {
                    group.is_empty() || group.iter().any(|(f, v)| e.get(f) == Some(v.as_str()))
                })
            };
            let Some(skip) = self.entries[self.at.min(self.entries.len())..].iter().position(passes) else {
                self.at = self.entries.len();
                return Ok(None);
            };
            self.at += skip + 1;
            self.read += 1;
            Ok(Some(self.entries[self.at - 1].clone()))
        }

        fn boots(&mut self) -> Result<Vec<Boot>> {
            Ok(self.boots.clone())
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
        let mut rec = Recorder::new(State::from_rows(&db.events_state().unwrap()), OWNER);
        src.filter = stream_filter();
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
                PID1[0],
                PID1[1],
                PID1[2],
            ],
        );
        src.add(400, &[("MESSAGE_ID", ids::SLEEP_START), ("_UID", "0")]);
        src.add(500, &[("MESSAGE_ID", ids::SLEEP_STOP), ("_UID", "0")]);
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
        assert_eq!(kinds, ["crash", "restart_loop"], "one loop, not one per read; one row per program's crashes");
        let data: Value = serde_json::from_str(&rows[1].4).unwrap();
        assert_eq!(data["n_restarts"], 4, "the row grows with the loop");
        assert_eq!((rows[1].0, rows[1].2.as_str()), (1000, "c1"));
        let crash: Value = serde_json::from_str(&rows[0].4).unwrap();
        assert_eq!((rows[0].3.as_str(), &crash["count"]), ("waybar crashed 2 times (SIGSEGV)", &json!(2)));

        run(&mut db, &mut src, 4000);
        assert_eq!(
            src.read, 1,
            "nothing open: only the last entry is seen again, and skipped"
        );
        assert_eq!(self::rows(&db).len(), 2);
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

    const LINUX: &str = "Linux version 6.9.0 (builder@host) #1 SMP PREEMPT_DYNAMIC";

    /// Six boots, each ended a different way, with noise the filtered reads must not wade through.
    fn ended_boots() -> Fake {
        let mut src = Fake {
            boots: ["a", "b", "c", "d", "e", "f"]
                .iter()
                .enumerate()
                .map(|(i, id)| boot(id, 1000 * i as u64 + 100, 1000 * i as u64 + 900))
                .collect(),
            ..Fake::default()
        };
        for (i, id) in ["a", "b", "c", "d", "e", "f"].iter().enumerate() {
            if *id != "e" {
                src.kernel(id, 1000 * i as u64 + 100, "5", LINUX);
            }
        }
        for t in 0..500 {
            src.add_in("b", 1200 + t, &[("MESSAGE", "noise"), ("PRIORITY", "6"), ("_UID", "1000")]);
        }
        // a: PID 1 stopping multi-user.target is a shutdown whoever started it, logind or not
        src.add_in(
            "a",
            890,
            &[[("MESSAGE_ID", ids::UNIT_STOPPED), ("UNIT", "multi-user.target")].as_slice(), &PID1].concat(),
        );
        // b: a forged shutdown, and PID 1 stopping some other unit, are not shutdowns; c's journald says why
        src.add_in("b", 1890, &[("MESSAGE_ID", ids::SHUTDOWN), ("_UID", "1000")]);
        src.add_in(
            "b",
            1895,
            &[[("MESSAGE_ID", ids::UNIT_STOPPED), ("UNIT", "x.service")].as_slice(), &PID1].concat(),
        );
        src.add_in(
            "c",
            2110,
            &[
                ("SYSLOG_IDENTIFIER", "systemd-journald"),
                ("PRIORITY", "4"),
                ("_UID", "0"),
                (
                    "MESSAGE",
                    "File /var/log/journal/0123/system.journal corrupted or uncleanly shut down, renaming and replacing.",
                ),
            ],
        );
        // c: d's kernel names a thermal trip
        src.kernel(
            "d",
            3101,
            "6",
            "x86/amd: Previous system reset reason [0x00000001]: thermal pin BP_THERMTRIP_L was tripped",
        );
        // d: its last sleep event is a suspend
        src.add_in("d", 3800, &[("MESSAGE_ID", ids::SLEEP_START), ("_UID", "0")]);
        // e: its start was vacuumed away
        src.add_in("e", 4800, &[("MESSAGE", "late"), ("PRIORITY", "6")]);
        src.entries.sort_by_key(|e| e.t_us);
        src
    }

    #[test]
    fn each_ended_boot_is_judged_on_its_markers_and_the_next_boots_evidence() {
        let mut src = ended_boots();
        let mut state = State::default();
        let events = boot_events(&mut src, &mut state, 0, true).unwrap();
        let ends: Vec<_> = events
            .iter()
            .filter(|e| e.kind == "boot_end")
            .map(|e| (e.key.as_str(), e.summary.as_str(), e.data["clean"].clone(), e.data["certainty"].clone()))
            .collect();
        assert_eq!(
            ends,
            [
                ("a", "Shut down", json!(true), json!("clean")),
                ("b", "Ended without shutdown", json!(false), json!("confirmed")),
                ("c", "Power lost: thermal trip", json!(false), json!("confirmed")),
                ("d", "Ended while suspended", json!(false), json!("ended_while_suspended")),
            ],
            "e's start is gone, so how it ended is unknown and gets no row; f is running"
        );
        let c = events.iter().find(|e| e.kind == "boot_end" && e.key == "c").unwrap();
        assert_eq!((&c.data["reset_reason"], &c.data["suspended"]), (&json!("thermal trip"), &json!(false)));
        assert_eq!(events.iter().filter(|e| e.kind == "boot").count(), 6);
        assert!(src.read < 50, "read {} entries: only markers and boot starts, never b's noise", src.read);
        assert_eq!(src.filter, stream_filter(), "the stream reads on with its own filter");
        assert!(boot_events(&mut src, &mut state, 0, true).unwrap().is_empty(), "each boot is judged once");

        let mut own = State::default();
        let events = boot_events(&mut ended_boots(), &mut own, 0, false).unwrap();
        assert!(
            events.iter().all(|e| e.kind == "boot"),
            "without the system journal every boot would look abrupt"
        );
    }

    #[test]
    fn a_unit_failure_forged_by_another_user_writes_no_event() {
        let mut src = Fake::default();
        let failure = |uid| {
            [
                ("MESSAGE_ID", ids::UNIT_FAILURE),
                ("UNIT", "backup.service"),
                ("UNIT_RESULT", "exit-code"),
                ("_PID", "4242"),
                ("_COMM", "systemd"),
                ("_UID", uid),
            ]
        };
        src.add(100, &failure("1001"));
        src.add(200, &[("MESSAGE_ID", ids::SLEEP_START), ("_UID", "1001")]);
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        run(&mut db, &mut src, 10_000);
        assert!(rows(&db).is_empty(), "another user can log any MESSAGE_ID and UNIT");

        src.add(300, &failure("1000"));
        run(&mut db, &mut src, 10_000);
        let kinds: Vec<_> = rows(&db).into_iter().map(|r| r.1).collect();
        assert_eq!(kinds, ["unit_failed"], "the owner's own user manager is believed");
    }

    #[test]
    fn a_kernel_segfault_at_info_priority_reaches_the_detector_and_debug_lines_are_not_read() {
        let mut src = Fake::default();
        src.kernel(
            BOOT,
            100,
            "6",
            "Hyprland[4242]: segfault at 0 ip 00007fef17a2d5e1 sp 00007fffd7cfb4e0 error 4 in libfoo.so.0[bc5e1,7fef1799c000+e3000]",
        );
        for t in 0..20 {
            src.kernel(BOOT, 110 + t, "7", "debug chatter");
        }
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        run(&mut db, &mut src, 10_000);
        let rows = rows(&db);
        assert_eq!(rows.len(), 1, "with no core dump the kernel's line is the only sign of the crash");
        let data: Value = serde_json::from_str(&rows[0].4).unwrap();
        assert_eq!((rows[0].3.as_str(), &data["source"]), ("Hyprland crashed (SIGSEGV)", &json!("kernel")));
        assert_eq!(src.read, 1, "debug lines are filtered out by the journal, not read");
    }

    #[test]
    fn a_crash_loop_is_one_row_whose_count_grows_and_a_reread_does_not_count_twice() {
        let mut src = Fake::default();
        for t in 0..10 {
            src.crash(100 + 40 * t, "Hyprland");
        }
        src.crash(600, "waybar");
        let dir = tempfile::tempdir().unwrap();
        let mut db = open(&dir);
        run(&mut db, &mut src, 10_000);
        let first = rows(&db);
        let got: Vec<_> = first.iter().map(|r| (r.0, r.3.as_str())).collect();
        assert_eq!(got, [(100, "Hyprland crashed 10 times (SIGSEGV)"), (600, "waybar crashed (SIGSEGV)")]);

        db.write(&json!({"type": "events", "t": 1.0, "events": [], "state": State::default().rows()}))
            .unwrap();
        run(&mut db, &mut src, 10_000);
        assert_eq!(rows(&db), first, "crashes read again are not new crashes");

        src.crash(2000, "Hyprland");
        run(&mut db, &mut src, 10_000);
        let data: Value = serde_json::from_str(&rows(&db)[0].4).unwrap();
        assert_eq!(data["count"], 11);
    }

    #[test]
    fn following_opens_the_files_that_can_still_grow_and_notices_a_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = [dir.path().to_path_buf()];
        let now = SystemTime::now();
        let file = |name: &str, age_s: u64| {
            let p = dir.path().join(name);
            std::fs::write(&p, "").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(now - Duration::from_secs(age_s))
                .unwrap();
        };
        file("system.journal", 0);
        file("user-1000.journal", 5);
        file("system@a-1-2.journal", 30 * 86400);
        file("system@b-3-4.journal", 60);
        file("notes.txt", 0);
        let read_us = now.duration_since(UNIX_EPOCH).unwrap().as_micros() as u64 - 120 * S;
        let growing: Vec<_> = growing_files(&dirs, read_us)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            growing,
            ["system.journal", "system@b-3-4.journal", "user-1000.journal"],
            "a file rotated after the last read may hold entries not yet read; month-old archives cannot"
        );

        let active = active_files(&dirs);
        assert_eq!(active.len(), 2, "archives are not written to");
        assert!(!rotated(&active));
        std::fs::rename(
            dir.path().join("system.journal"),
            dir.path().join("system@c-5-6.journal"),
        )
        .unwrap();
        file("system.journal", 0);
        assert!(rotated(&active), "the new system.journal is another file");
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
