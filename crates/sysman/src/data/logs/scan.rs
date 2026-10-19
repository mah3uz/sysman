use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use journal::Entry;
use journal::categories::{Categories, Category, INDEX_FIELDS, categorize, source};
use journal::findings::{Detector, FindingKind};
use journal::packages::{self, Manager, Transaction};
use journal::reader::{Backend, Boot, Fields, Filter, Journal, Seek};

use super::index::{Batch, Coverage, Index, Row};
use super::query::{Query, SearchSpec};

const PUBLISH_EVERY: Duration = Duration::from_millis(100);
const CHECK_EVERY: u64 = 512;
const FIELD_LIMIT: usize = 2048;
const HASH_BYTES: usize = 1024;
const LOCATE_WINDOW: usize = 64;
const PID_FIELDS: &[&str] = &["_PID", "COREDUMP_PID", "OBJECT_PID"];
const SOURCE_FIELDS: &[&str] = &[
    "_TRANSPORT",
    "_SYSTEMD_USER_UNIT",
    "_SYSTEMD_UNIT",
    "SYSLOG_IDENTIFIER",
    "_COMM",
    "MESSAGE",
];

/// What a scan covers: one span, plus filters the journal applies so less is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub span: Span,
    pub narrow: Narrow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    /// `to_us` of `None` runs to the end of the journal.
    Range { from_us: u64, to_us: Option<u64> },
    /// A boot by its `_BOOT_ID`.
    Boot(String),
}

/// Journal-side narrowing from the query's `unit:`, `source:` and `prio:`. With any of it, the index holds only
/// those entries, no package rows are added and findings are incomplete, so changing it means a new scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Narrow {
    pub units: Vec<String>,
    pub sources: Vec<String>,
    pub max_priority: Option<u8>,
}

impl Narrow {
    pub fn from_query(q: &Query) -> Narrow {
        Narrow {
            units: q.units.clone(),
            sources: q.sources.clone(),
            max_priority: q.max_priority,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty() && self.sources.is_empty() && self.max_priority.is_none_or(|p| p >= 6)
    }

    fn add_to(&self, mut f: Filter) -> Filter {
        if !self.units.is_empty() {
            let terms = self.units.iter().flat_map(|u| {
                let u = u.strip_suffix(".service").unwrap_or(u);
                ["_SYSTEMD_UNIT", "_SYSTEMD_USER_UNIT", "UNIT"]
                    .into_iter()
                    .flat_map(move |field| [(field, format!("{u}.service")), (field, u.to_string())])
            });
            f = f.any(terms);
        }
        if !self.sources.is_empty() {
            f = f.any(self.sources.iter().map(|s| ("SYSLOG_IDENTIFIER", s.clone())));
        }
        if let Some(p) = self.max_priority.filter(|p| *p < 6) {
            f = f.any((0..=p).map(|n| ("PRIORITY", n.to_string())));
        }
        f
    }
}

impl Scope {
    /// From `from_us` (microseconds since the epoch) to the end of the journal.
    pub fn since(from_us: u64) -> Scope {
        Scope {
            span: Span::Range { from_us, to_us: None },
            narrow: Narrow::default(),
        }
    }

    pub fn range(from_us: u64, to_us: u64) -> Scope {
        Scope {
            span: Span::Range {
                from_us,
                to_us: Some(to_us),
            },
            narrow: Narrow::default(),
        }
    }

    pub fn boot(id: &str) -> Scope {
        Scope {
            span: Span::Boot(id.to_string()),
            narrow: Narrow::default(),
        }
    }

    /// The running boot, the default scope.
    pub fn this_boot() -> Scope {
        Scope::boot(&current_boot_id())
    }

    pub fn narrowed(mut self, narrow: Narrow) -> Scope {
        self.narrow = narrow;
        self
    }

    fn filter(&self) -> Filter {
        let f = match &self.span {
            Span::Boot(id) => Filter::default().any([("_BOOT_ID", id.as_str())]),
            Span::Range { .. } => Filter::default(),
        };
        self.narrow.add_to(f)
    }
}

pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

fn current_boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().replace('-', ""))
        .unwrap_or_default()
}

/// Hash of the source and the first KiB of the message: what makes two entries "the same message".
pub fn row_hash(source: &str, message: &str) -> u64 {
    let mut end = message.len().min(HASH_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in source.bytes().chain([0xff]).chain(message[..end].bytes()) {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

pub(super) fn entry_hash(e: &Entry) -> u64 {
    row_hash(source(e), e.message())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanState {
    Running,
    Done,
    Cancelled,
    Failed(String),
}

impl ScanState {
    pub fn is_running(&self) -> bool {
        *self == ScanState::Running
    }
}

/// Turns entries into rows, interning strings; its output is published to the shared index in batches.
#[derive(Default)]
pub(super) struct Builder {
    names: HashMap<String, u32>,
    boots: HashMap<String, u16>,
    out: Batch,
    published: usize,
    transactions: usize,
}

impl Builder {
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(id) = self.names.get(s) {
            return *id;
        }
        let id = self.names.len() as u32;
        self.names.insert(s.to_string(), id);
        self.out.names.push(s.to_string());
        id
    }

    fn boot(&mut self, id: &str) -> u16 {
        if let Some(o) = self.boots.get(id) {
            return *o;
        }
        let o = self.boots.len() as u16;
        self.boots.insert(id.to_string(), o);
        self.out.boot_ids.push(id.to_string());
        o
    }

    fn next_id(&self) -> usize {
        self.published + self.out.rows.len()
    }

    pub(super) fn add_entry(&mut self, e: &Entry) {
        let src = source(e);
        let row = Row {
            t_us: e.t_us,
            hash: row_hash(src, e.message()),
            source: self.intern(src),
            extra: self.intern(e.get("SYSLOG_IDENTIFIER").unwrap_or_default()),
            categories: categorize(e),
            boot: self.boot(e.get("_BOOT_ID").unwrap_or_default()),
            priority: e.priority(),
            package: false,
        };
        self.out.rows.push(row);
    }

    fn add_package(&mut self, t: Transaction, boots: &[Boot]) {
        let boot_id = boots
            .iter()
            .rev()
            .find(|b| b.first_us <= t.start_us)
            .or(boots.first())
            .map_or("", |b| b.id.as_str())
            .to_string();
        let mut categories = Categories::default();
        categories.insert(Category::Packages);
        let manager = match t.manager {
            Manager::Pacman => "pacman",
            Manager::Dpkg => "dpkg",
            Manager::Apt => "apt",
        };
        let row = Row {
            t_us: t.start_us,
            hash: self.transactions as u64,
            source: self.intern(manager),
            extra: self.transactions as u32,
            categories,
            boot: self.boot(&boot_id),
            priority: 6,
            package: true,
        };
        self.transactions += 1;
        self.out.rows.push(row);
        self.out.transactions.push(t);
    }

    pub(super) fn take(&mut self) -> Batch {
        self.published += self.out.rows.len();
        std::mem::take(&mut self.out)
    }
}

/// A running or finished scan: the index it fills, its progress and its cancel flag. Read the index at any time;
/// it grows in batches about every 100 ms while [`Scan::state`] is `Running`.
pub struct Scan {
    scope: Scope,
    index: RwLock<Index>,
    entries: AtomicU64,
    at_us: AtomicU64,
    cancel: AtomicBool,
    state: Mutex<ScanState>,
    backend: Mutex<Option<Backend>>,
    started: Instant,
    elapsed_ms: AtomicU64,
}

impl Scan {
    /// Starts a scan of `scope` on its own thread.
    pub fn start(scope: Scope) -> Arc<Scan> {
        let scan = Arc::new(Scan {
            scope,
            index: RwLock::default(),
            entries: AtomicU64::new(0),
            at_us: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            state: Mutex::new(ScanState::Running),
            backend: Mutex::new(None),
            started: Instant::now(),
            elapsed_ms: AtomicU64::new(0),
        });
        let worker = scan.clone();
        let spawned = thread::Builder::new().name("logs-scan".into()).spawn(move || {
            let result = worker.run();
            worker
                .elapsed_ms
                .store(worker.started.elapsed().as_millis() as u64, Ordering::Relaxed);
            let state = match result {
                Err(e) => ScanState::Failed(format!("{e:#}")),
                Ok(()) if worker.cancelled() => ScanState::Cancelled,
                Ok(()) => ScanState::Done,
            };
            *worker.state.lock().unwrap() = state;
        });
        if let Err(e) = spawned {
            *scan.state.lock().unwrap() = ScanState::Failed(e.to_string());
        }
        scan
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// The index so far. Hold the guard only while computing, not across frames: the scan thread waits on it to
    /// publish.
    pub fn index(&self) -> RwLockReadGuard<'_, Index> {
        self.index.read().unwrap()
    }

    pub fn state(&self) -> ScanState {
        self.state.lock().unwrap().clone()
    }

    /// Journal entries read so far (package rows not counted), which may be ahead of the published rows by up to
    /// one batch.
    pub fn entries_read(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    /// Timestamp of the entry read last; 0 before the first.
    pub fn position_us(&self) -> u64 {
        self.at_us.load(Ordering::Relaxed)
    }

    /// Wall time the scan took once it ended; 0 while running.
    pub fn elapsed(&self) -> Duration {
        Duration::from_millis(self.elapsed_ms.load(Ordering::Relaxed))
    }

    /// The reader in use once the scan has opened it.
    pub fn backend(&self) -> Option<Backend> {
        *self.backend.lock().unwrap()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn publish(&self, b: &mut Builder) {
        let batch = b.take();
        self.index.write().unwrap().append(batch);
    }

    fn open(&self) -> Result<(Journal, Vec<Boot>, Seek)> {
        let mut j = Journal::open()?;
        *self.backend.lock().unwrap() = Some(j.backend());
        let boots = j.boots()?;
        j.set_filter(&self.scope.filter())?;
        j.set_data_threshold(FIELD_LIMIT)?;
        let seek = match &self.scope.span {
            Span::Range { from_us, .. } if *from_us > 0 => Seek::Realtime(*from_us),
            _ => Seek::Head,
        };
        Ok((j, boots, seek))
    }

    fn upper_bound(&self) -> Option<u64> {
        match self.scope.span {
            Span::Range { to_us, .. } => to_us,
            Span::Boot(_) => None,
        }
    }

    fn run(&self) -> Result<()> {
        let (mut j, boots, seek) = self.open()?;
        let current = current_boot_id();
        let (pkg_from, pkg_to, coverage) = self.bounds(&boots, &current);
        self.index
            .write()
            .unwrap()
            .begin(boots.clone(), current.clone(), coverage);
        let mut packages = if self.scope.narrow.is_empty() {
            let mut t = packages::read_history(pkg_from).unwrap_or_default();
            t.retain(|t| t.start_us >= pkg_from && t.start_us <= pkg_to);
            t
        } else {
            vec![]
        }
        .into_iter()
        .peekable();

        j.seek(seek)?;
        let to = self.upper_bound();
        let mut b = Builder::default();
        let mut detector = Detector::new();
        let mut last = Instant::now();
        let mut n = 0u64;
        while let Some(e) = j.next(Fields::Only(INDEX_FIELDS))? {
            if self.cancelled() || to.is_some_and(|t| e.t_us > t) {
                break;
            }
            while let Some(t) = packages.next_if(|t| t.start_us <= e.t_us) {
                b.add_package(t, &boots);
            }
            detector.push(b.next_id(), &e);
            b.add_entry(&e);
            n += 1;
            if n.is_multiple_of(CHECK_EVERY) {
                self.entries.store(n, Ordering::Relaxed);
                self.at_us.store(e.t_us, Ordering::Relaxed);
                if last.elapsed() >= PUBLISH_EVERY {
                    self.publish(&mut b);
                    last = Instant::now();
                }
            }
        }
        self.entries.store(n, Ordering::Relaxed);
        if self.cancelled() {
            return Ok(());
        }
        for t in packages {
            b.add_package(t, &boots);
        }
        self.publish(&mut b);
        let mut findings = detector.finish(Some(&current));
        if let Some(to) = to {
            findings.retain(|f| {
                f.kind != FindingKind::BootWithoutShutdown
                    || boots.iter().find(|b| b.id == f.boot_id).is_none_or(|b| b.last_us <= to)
            });
        }
        self.index.write().unwrap().set_findings(findings);
        Ok(())
    }

    fn bounds(&self, boots: &[Boot], current: &str) -> (u64, u64, Coverage) {
        let unfiltered = self.scope.narrow.is_empty();
        let (from_us, to_us) = match &self.scope.span {
            Span::Range { from_us, to_us } => (*from_us, to_us.unwrap_or(u64::MAX)),
            Span::Boot(id) => match boots.iter().find(|b| b.id == *id) {
                Some(b) if b.id == current => (b.first_us, u64::MAX),
                Some(b) => (b.first_us, b.last_us),
                None => (0, 0),
            },
        };
        (
            from_us,
            to_us,
            Coverage {
                from_us,
                to_us,
                unfiltered,
            },
        )
    }
}

enum Locate {
    Found(usize),
    Missing,
    NeedMore,
}

/// Finds the row for an entry read in index order: from `from`, the first journal row with the same time and
/// hash. Looking only a short way ahead, and in order, is what keeps entries with equal timestamps matched to
/// their own rows: the n-th identical (time, hash) entry read is the n-th such row.
fn locate(rows: &[Row], from: usize, t_us: u64, hash: u64) -> Locate {
    let mut seen = 0;
    for (i, r) in rows.iter().enumerate().skip(from) {
        if r.package {
            continue;
        }
        if r.t_us == t_us && r.hash == hash {
            return Locate::Found(i);
        }
        seen += 1;
        if seen >= LOCATE_WINDOW {
            return Locate::Missing;
        }
    }
    Locate::NeedMore
}

/// A streaming search over the scan's scope: it re-reads the journal in the same order as the scan, checks each
/// message (and pid) against the query's free text and records the ids of matching rows. Hits arrive in
/// ascending row order about every 100 ms; package rows whose package names match are added when it ends.
///
/// A message is tied to its index row by (timestamp, hash of source and message), walking both in the same
/// order, so equal timestamps resolve by position.
pub struct Search {
    hits: RwLock<Vec<u32>>,
    rows_done: AtomicU64,
    cancel: AtomicBool,
    state: Mutex<ScanState>,
}

impl Search {
    /// Starts streaming for `spec` over `scan`'s scope.
    pub fn start(scan: Arc<Scan>, spec: SearchSpec) -> Arc<Search> {
        let search = Arc::new(Search {
            hits: RwLock::default(),
            rows_done: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            state: Mutex::new(ScanState::Running),
        });
        let worker = search.clone();
        let spawned = thread::Builder::new().name("logs-search".into()).spawn(move || {
            let result = worker.run(&scan, &spec);
            let state = match result {
                Err(e) => ScanState::Failed(format!("{e:#}")),
                Ok(()) if worker.cancelled() => ScanState::Cancelled,
                Ok(()) => ScanState::Done,
            };
            *worker.state.lock().unwrap() = state;
        });
        if let Err(e) = spawned {
            *search.state.lock().unwrap() = ScanState::Failed(e.to_string());
        }
        search
    }

    /// Matching row ids so far, ascending; pass to the index queries as `hits`.
    pub fn hits(&self) -> RwLockReadGuard<'_, Vec<u32>> {
        self.hits.read().unwrap()
    }

    /// Index rows the search has got through.
    pub fn rows_done(&self) -> u64 {
        self.rows_done.load(Ordering::Relaxed)
    }

    pub fn state(&self) -> ScanState {
        self.state.lock().unwrap().clone()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn run(&self, scan: &Scan, spec: &SearchSpec) -> Result<()> {
        let (mut j, _, seek) = scan.open()?;
        j.seek(seek)?;
        let to = scan.upper_bound();
        let mut fields: Vec<&str> = SOURCE_FIELDS.to_vec();
        if !spec.pids.is_empty() {
            fields.extend(PID_FIELDS);
        }
        let mut pending = vec![];
        let mut last = Instant::now();
        let mut at = 0usize;
        let mut n = 0u64;
        while let Some(e) = j.next(Fields::Only(&fields))? {
            if self.cancelled() || to.is_some_and(|t| e.t_us > t) {
                break;
            }
            let hash = entry_hash(&e);
            let row = loop {
                let finished = !scan.state().is_running();
                let found = locate(scan.index().rows(), at, e.t_us, hash);
                match found {
                    Locate::Found(i) => break Some(i),
                    Locate::Missing => break None,
                    Locate::NeedMore if finished || self.cancelled() => break None,
                    Locate::NeedMore => thread::sleep(Duration::from_millis(20)),
                }
            };
            if let Some(i) = row {
                at = i + 1;
                if spec.matches(&e) {
                    pending.push(i as u32);
                }
            }
            n += 1;
            if n.is_multiple_of(CHECK_EVERY) && last.elapsed() >= PUBLISH_EVERY {
                self.flush(&mut pending, at);
                last = Instant::now();
            }
            if at >= scan.index().len() && !scan.state().is_running() {
                break;
            }
        }
        if self.cancelled() {
            return Ok(());
        }
        if spec.pids.is_empty() {
            let index = scan.index();
            pending.extend(
                index
                    .rows()
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| r.package)
                    .filter(|(_, r)| index.transaction(r).is_some_and(|t| spec.matches_package(t)))
                    .map(|(i, _)| i as u32),
            );
        }
        self.flush(&mut pending, scan.index().len());
        self.hits.write().unwrap().sort_unstable();
        Ok(())
    }

    fn flush(&self, pending: &mut Vec<u32>, done: usize) {
        self.hits.write().unwrap().append(pending);
        self.rows_done.store(done as u64, Ordering::Relaxed);
    }
}

impl SearchSpec {
    /// Every term in the message, case-insensitively, and, when pids are given, one of them in a pid field.
    pub fn matches(&self, e: &Entry) -> bool {
        let message = e.message();
        let found = |term: &String| {
            if term.is_ascii() {
                contains_ascii_nocase(message, term)
            } else {
                message.to_lowercase().contains(term.as_str())
            }
        };
        self.terms.iter().all(found)
            && (self.pids.is_empty()
                || PID_FIELDS
                    .iter()
                    .filter_map(|f| e.get(f)?.parse::<u32>().ok())
                    .any(|p| self.pids.contains(&p)))
    }

    fn matches_package(&self, t: &Transaction) -> bool {
        let names: String = t
            .changes
            .iter()
            .map(|c| c.name.to_lowercase())
            .collect::<Vec<_>>()
            .join(" ");
        !self.terms.is_empty() && self.terms.iter().all(|term| names.contains(term.as_str()))
    }
}

fn contains_ascii_nocase(haystack: &str, lower_needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), lower_needle.as_bytes());
    n.is_empty() || (h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n)))
}

#[cfg(test)]
mod tests {
    use super::super::index::tests::{entry, kernel};
    use super::*;

    fn spec(terms: &[&str], pids: &[u32]) -> SearchSpec {
        SearchSpec {
            terms: terms.iter().map(|t| t.to_string()).collect(),
            pids: pids.to_vec(),
        }
    }

    #[test]
    fn free_text_matches_every_term_ignoring_case() {
        let e = kernel(1, 6, "Disk Error on /dev/sda");
        assert!(spec(&["disk", "error"], &[]).matches(&e));
        assert!(!spec(&["disk", "warning"], &[]).matches(&e));
        assert!(
            spec(&["\u{e9}t\u{e9}"], &[]).matches(&kernel(1, 6, "\u{c9}T\u{c9} started")),
            "non-ASCII too"
        );
    }

    #[test]
    fn a_pid_may_be_any_of_the_pid_fields() {
        let e = entry(1, 6, "x", "m", &[("COREDUMP_PID", "42")]);
        assert!(spec(&[], &[7, 42]).matches(&e));
        assert!(!spec(&[], &[7]).matches(&e));
        assert!(spec(&["m"], &[42]).matches(&e) && !spec(&["zzz"], &[42]).matches(&e));
    }

    fn rows_of(entries: &[Entry]) -> Vec<Row> {
        let mut b = Builder::default();
        entries.iter().for_each(|e| b.add_entry(e));
        b.take().rows
    }

    #[test]
    fn the_search_finds_each_of_several_entries_sharing_a_timestamp() {
        let entries = [
            entry(5, 6, "a", "x", &[]),
            entry(5, 6, "a", "y", &[]),
            entry(5, 6, "a", "x", &[]),
        ];
        let rows = rows_of(&entries);
        let mut at = 0;
        let mut found = vec![];
        for e in &entries {
            let Locate::Found(i) = locate(&rows, at, e.t_us, entry_hash(e)) else {
                panic!()
            };
            found.push(i);
            at = i + 1;
        }
        assert_eq!(found, [0, 1, 2], "the second x is its own row, not the first again");
    }

    #[test]
    fn an_entry_missing_from_the_index_is_skipped_without_losing_the_cursor() {
        let rows = rows_of(&[entry(1, 6, "a", "x", &[]), entry(3, 6, "a", "z", &[])]);
        let ghost = entry(2, 6, "a", "new", &[]);
        assert!(matches!(
            locate(&rows, 1, ghost.t_us, entry_hash(&ghost)),
            Locate::NeedMore
        ));
        let z = entry(3, 6, "a", "z", &[]);
        assert!(matches!(locate(&rows, 1, z.t_us, entry_hash(&z)), Locate::Found(1)));
    }

    #[test]
    fn the_hash_depends_on_source_and_message_only() {
        assert_eq!(row_hash("a", "m"), row_hash("a", "m"));
        assert_ne!(row_hash("a", "m"), row_hash("b", "m"));
        assert_ne!(
            row_hash("ab", "m"),
            row_hash("a", "bm"),
            "the boundary is part of the hash"
        );
        let long = "x".repeat(HASH_BYTES + 500);
        assert_eq!(
            row_hash("a", &long),
            row_hash("a", &long[..HASH_BYTES + 10]),
            "only a prefix is read"
        );
    }

    #[test]
    fn the_scope_filter_translates_tokens_for_the_journal() {
        let q = Query::parse("unit:ssh.service prio:warning source:foo", 0, &jiff::tz::TimeZone::UTC);
        let f = Scope::boot("abc").narrowed(Narrow::from_query(&q)).filter();
        assert_eq!(f.0.len(), 4, "boot, unit, source, priority");
        assert!(f.0[1].contains(&("_SYSTEMD_UNIT".to_string(), "ssh.service".to_string())));
        assert!(f.0[1].contains(&("UNIT".to_string(), "ssh".to_string())));
        assert_eq!(f.0[3].len(), 5, "priorities 0 to 4");
        assert!(Narrow::from_query(&Query::default()).is_empty());
        let info = Narrow {
            max_priority: Some(6),
            ..Default::default()
        };
        assert!(info.is_empty(), "info and above is everything");
    }

    #[test]
    fn a_new_scope_cancels_the_old_scan() {
        let mut logs = super::super::Logs::default();
        let first = logs.scan(Scope::since(now_us() - 60_000_000));
        let second = logs.scan(Scope::since(now_us() - 120_000_000));
        assert!(first.cancelled(), "starting a scan flags the previous one");
        assert!(!second.cancelled());
        let deadline = Instant::now() + Duration::from_secs(20);
        while first.state().is_running() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!first.state().is_running(), "and it stops: {:?}", first.state());
        logs.cancel();
        assert!(second.cancelled());
    }
}
