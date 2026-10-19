use std::collections::HashMap;

use journal::categories::{Categories, Category};
use journal::findings::{Finding, FindingKind};
use journal::packages::Transaction;
use journal::reader::Boot;

use super::query::{BootRef, Query};

/// One journal entry or package transaction, 32 bytes. Rows stay in scan order (journal order), which is time
/// order except across clock jumps, so time ranges are tested row by row rather than by bisection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub t_us: u64,
    /// Hash of source and message (first 1 KiB): equal for repeats, and with `t_us` it identifies the entry.
    pub hash: u64,
    /// Interned display source, see [`Index::name`].
    pub source: u32,
    /// Journal rows: interned syslog identifier. Package rows: index into [`Index::transactions`].
    pub extra: u32,
    pub categories: Categories,
    /// Index into [`Index::boot_ids`].
    pub boot: u16,
    /// Syslog priority 0..=7.
    pub priority: u8,
    pub package: bool,
}

const _: () = assert!(std::mem::size_of::<Row>() <= 32);

/// Rows added since the last publish.
#[derive(Default)]
pub(super) struct Batch {
    pub rows: Vec<Row>,
    pub names: Vec<String>,
    pub boot_ids: Vec<String>,
    pub transactions: Vec<Transaction>,
}

/// What a scan has seen of the scope's time span; decides whether boot tallies can be trusted.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Coverage {
    pub from_us: u64,
    pub to_us: u64,
    pub unfiltered: bool,
}

#[derive(Default)]
pub struct Index {
    rows: Vec<Row>,
    names: Vec<String>,
    name_ids: HashMap<String, u32>,
    boot_ids: Vec<String>,
    boots: Vec<Boot>,
    current_boot: String,
    transactions: Vec<Transaction>,
    findings: Vec<Finding>,
    findings_ready: bool,
    coverage: Coverage,
}

/// One visible list row: `row` is the first of `count` consecutive identical rows, `last` the final one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Visible {
    pub row: u32,
    pub last: u32,
    pub count: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Matches {
    /// In index order (oldest first).
    pub rows: Vec<Visible>,
    /// Entries behind the visible rows, folded repeats included.
    pub entries: u64,
}

/// Entries per category. A row in several categories is counted in each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CategoryCounts {
    pub all: u64,
    by: [u64; 9],
}

impl CategoryCounts {
    pub fn get(&self, c: Category) -> u64 {
        self.by[c as usize]
    }
}

/// Entries in one time bucket by priority band.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bucket {
    /// Priority 0 to 3.
    pub errors: u32,
    pub warnings: u32,
    pub notices: u32,
    /// Priority 6 and 7.
    pub info: u32,
}

impl Bucket {
    pub fn total(&self) -> u32 {
        self.errors + self.warnings + self.notices + self.info
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub category: Category,
    pub counts: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootEnd {
    /// The boot in progress.
    Running,
    Clean,
    /// No shutdown message and no journal-stopped entry.
    Abrupt,
    /// The scan did not cover the whole boot, or has not finished.
    Unknown,
}

/// A boot for the sidebar, with tallies from the findings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootSummary {
    pub id: String,
    pub first_us: u64,
    pub last_us: u64,
    pub current: bool,
    pub end: BootEnd,
    pub crashes: usize,
    pub failed_units: usize,
    pub oom_kills: usize,
}

#[derive(Clone, Copy)]
struct Skip {
    category: bool,
    time: bool,
}

impl Skip {
    const NONE: Skip = Skip {
        category: false,
        time: false,
    };
    const CATEGORY: Skip = Skip {
        category: true,
        time: false,
    };
    const TIME: Skip = Skip {
        category: false,
        time: true,
    };
    const BOTH: Skip = Skip {
        category: true,
        time: true,
    };
}

struct Plan<'a> {
    q: &'a Query,
    hits: Option<&'a [u32]>,
    skip: Skip,
    sources: Option<Vec<u32>>,
    identifiers: Option<Vec<u32>>,
    boots: Option<Vec<u16>>,
}

impl Plan<'_> {
    fn test(&self, id: usize, r: &Row) -> bool {
        let q = self.q;
        if q.max_priority.is_some_and(|p| r.priority > p) {
            return false;
        }
        if !self.skip.category && !q.categories.is_empty() && !q.categories.iter().any(|c| r.categories.contains(*c)) {
            return false;
        }
        if !self.skip.time && (q.since_us.is_some_and(|t| r.t_us < t) || q.until_us.is_some_and(|t| r.t_us > t)) {
            return false;
        }
        if self.boots.as_ref().is_some_and(|b| !b.contains(&r.boot))
            || self.sources.as_ref().is_some_and(|s| !s.contains(&r.source))
            || self
                .identifiers
                .as_ref()
                .is_some_and(|i| r.package || !i.contains(&r.extra))
        {
            return false;
        }
        self.hits.is_none_or(|h| h.binary_search(&(id as u32)).is_ok())
    }
}

impl Index {
    pub(super) fn append(&mut self, b: Batch) {
        for n in b.names {
            self.name_ids.insert(n.clone(), self.names.len() as u32);
            self.names.push(n);
        }
        self.boot_ids.extend(b.boot_ids);
        self.transactions.extend(b.transactions);
        self.rows.extend(b.rows);
    }

    pub(super) fn begin(&mut self, boots: Vec<Boot>, current_boot: String, coverage: Coverage) {
        self.boots = boots;
        self.current_boot = current_boot;
        self.coverage = coverage;
    }

    pub(super) fn set_findings(&mut self, findings: Vec<Finding>) {
        self.findings = findings;
        self.findings_ready = true;
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn row(&self, id: u32) -> Option<&Row> {
        self.rows.get(id as usize)
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// An interned string: a display source (`row.source`) or syslog identifier.
    pub fn name(&self, id: u32) -> &str {
        self.names.get(id as usize).map_or("", String::as_str)
    }

    /// The syslog identifier of a journal row; empty for packages.
    pub fn identifier(&self, r: &Row) -> &str {
        if r.package { "" } else { self.name(r.extra) }
    }

    /// `_BOOT_ID` of a row's boot.
    pub fn boot_id(&self, r: &Row) -> &str {
        self.boot_ids.get(r.boot as usize).map_or("", String::as_str)
    }

    /// The transaction behind a package row, with every change kept.
    pub fn transaction(&self, r: &Row) -> Option<&Transaction> {
        r.package.then(|| self.transactions.get(r.extra as usize)).flatten()
    }

    pub fn transactions(&self) -> &[Transaction] {
        &self.transactions
    }

    /// Empty until the scan finishes: the rules need the whole stream. Each finding's `entries` are row ids.
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// Every boot in the journal, oldest first, whatever the scope.
    pub fn boots(&self) -> &[Boot] {
        &self.boots
    }

    /// Approximate heap bytes held.
    pub fn memory_bytes(&self) -> usize {
        self.rows.capacity() * std::mem::size_of::<Row>()
            + self.names.iter().map(|n| n.len() + 24 + 48).sum::<usize>()
            + self.findings.len() * 512
    }

    /// Boots for the sidebar, oldest first. A boot's end and tallies are known only when the scan finished
    /// without a narrowing filter and covered the whole boot.
    pub fn boot_summaries(&self) -> Vec<BootSummary> {
        let c = self.coverage;
        self.boots
            .iter()
            .map(|b| {
                let current = b.id == self.current_boot;
                let covered =
                    self.findings_ready && c.unfiltered && c.from_us <= b.first_us && (current || c.to_us >= b.last_us);
                let mut s = BootSummary {
                    id: b.id.clone(),
                    first_us: b.first_us,
                    last_us: b.last_us,
                    current,
                    end: BootEnd::Unknown,
                    crashes: 0,
                    failed_units: 0,
                    oom_kills: 0,
                };
                if !covered {
                    return s;
                }
                s.end = if current { BootEnd::Running } else { BootEnd::Clean };
                for f in self.findings.iter().filter(|f| f.boot_id == b.id) {
                    match f.kind {
                        FindingKind::BootWithoutShutdown => s.end = BootEnd::Abrupt,
                        FindingKind::Crash => s.crashes += f.count,
                        FindingKind::FailedUnit => s.failed_units += f.count,
                        FindingKind::OomKill => s.oom_kills += f.count,
                        _ => {}
                    }
                }
                s
            })
            .collect()
    }

    fn boot_ordinals(&self, refs: &[BootRef]) -> Vec<u16> {
        let here = self
            .boots
            .iter()
            .position(|b| b.id == self.current_boot)
            .or(self.boots.len().checked_sub(1));
        let ids = refs.iter().filter_map(|r| match r {
            BootRef::Id(id) => Some(id.as_str()),
            BootRef::Back(n) => {
                let at = here?.checked_sub(*n as usize)?;
                self.boots.get(at).map(|b| b.id.as_str())
            }
        });
        ids.filter_map(|id| self.boot_ids.iter().position(|b| b == id))
            .map(|o| o as u16)
            .collect()
    }

    fn names_to_ids(&self, wanted: &[String], strip_service: bool) -> Vec<u32> {
        wanted
            .iter()
            .filter_map(|n| {
                let n = if strip_service {
                    n.strip_suffix(".service").unwrap_or(n)
                } else {
                    n
                };
                self.name_ids.get(n).copied()
            })
            .collect()
    }

    fn plan<'a>(&self, q: &'a Query, hits: Option<&'a [u32]>, skip: Skip) -> Plan<'a> {
        Plan {
            q,
            hits,
            skip,
            sources: (!q.units.is_empty()).then(|| self.names_to_ids(&q.units, true)),
            identifiers: (!q.sources.is_empty()).then(|| self.names_to_ids(&q.sources, false)),
            boots: (!q.boots.is_empty()).then(|| self.boot_ordinals(&q.boots)),
        }
    }

    fn for_each(&self, plan: &Plan, mut f: impl FnMut(u32, &Row)) {
        match plan.q.finding {
            Some(i) => {
                let mut ids = self.findings.get(i).map_or(vec![], |f| f.entries.clone());
                ids.sort_unstable();
                for id in ids {
                    if let Some(r) = self.rows.get(id).filter(|r| plan.test(id, r)) {
                        f(id as u32, r);
                    }
                }
            }
            None => {
                for (id, r) in self.rows.iter().enumerate() {
                    if plan.test(id, r) {
                        f(id as u32, r);
                    }
                }
            }
        }
    }

    /// The rows a query shows. `hits` are the ascending row ids from a finished or running [`Search`], required
    /// when [`Query::search`] is `Some` (rows not yet reached by a running search are not shown). Consecutive
    /// matching rows with the same hash and boot fold into one [`Visible`]; package rows never fold.
    ///
    /// [`Search`]: super::Search
    pub fn matching(&self, q: &Query, hits: Option<&[u32]>) -> Matches {
        let plan = self.plan(q, hits, Skip::NONE);
        let mut out = Matches::default();
        let mut prev: Option<(u64, u16)> = None;
        self.for_each(&plan, |id, r| {
            out.entries += 1;
            let same = prev == Some((r.hash, r.boot)) && !r.package;
            match out.rows.last_mut() {
                Some(v) if same => {
                    v.count += 1;
                    v.last = id;
                }
                _ => out.rows.push(Visible {
                    row: id,
                    last: id,
                    count: 1,
                }),
            }
            prev = Some((r.hash, r.boot));
        });
        out
    }

    /// Entries per category under the query with its own category filter left out, so each count is what
    /// clicking that category would show.
    pub fn category_counts(&self, q: &Query, hits: Option<&[u32]>) -> CategoryCounts {
        let plan = self.plan(q, hits, Skip::CATEGORY);
        let mut out = CategoryCounts::default();
        self.for_each(&plan, |_, r| {
            out.all += 1;
            for c in Category::ALL {
                if r.categories.contains(c) {
                    out.by[c as usize] += 1;
                }
            }
        });
        out
    }

    /// `buckets` equal slices of `from_us..to_us` with entries by priority band. The query's own time range is
    /// ignored (this is what picks it); everything else applies.
    pub fn histogram(&self, q: &Query, hits: Option<&[u32]>, from_us: u64, to_us: u64, buckets: usize) -> Vec<Bucket> {
        let plan = self.plan(q, hits, Skip::TIME);
        let mut out = vec![Bucket::default(); buckets];
        self.for_each(&plan, |_, r| {
            if let Some(b) = bucket_of(r.t_us, from_us, to_us, buckets) {
                let b = &mut out[b];
                match r.priority {
                    0..=3 => b.errors += 1,
                    4 => b.warnings += 1,
                    5 => b.notices += 1,
                    _ => b.info += 1,
                }
            }
        });
        out
    }

    /// One lane per category with entries per bucket, under the query without its category and time filters.
    pub fn lanes(&self, q: &Query, hits: Option<&[u32]>, from_us: u64, to_us: u64, buckets: usize) -> Vec<Lane> {
        let plan = self.plan(q, hits, Skip::BOTH);
        let mut lanes: Vec<Lane> = Category::ALL
            .into_iter()
            .map(|category| Lane {
                category,
                counts: vec![0; buckets],
            })
            .collect();
        self.for_each(&plan, |_, r| {
            if let Some(b) = bucket_of(r.t_us, from_us, to_us, buckets) {
                for (lane, c) in lanes.iter_mut().zip(Category::ALL) {
                    if r.categories.contains(c) {
                        lane.counts[b] += 1;
                    }
                }
            }
        });
        lanes
    }

    /// The first journal row of `id`'s group of equal (time, hash) rows counted back from it: how many rows with
    /// the same timestamp and hash come before `id`. Entries that cannot be told apart by those two are taken in
    /// journal order, so this is the position to pick among them.
    pub fn occurrence(&self, id: u32) -> usize {
        let Some(r) = self.row(id) else { return 0 };
        self.rows[..id as usize]
            .iter()
            .rev()
            .take_while(|p| p.t_us == r.t_us)
            .filter(|p| p.hash == r.hash && !p.package)
            .count()
    }
}

fn bucket_of(t: u64, from: u64, to: u64, n: usize) -> Option<usize> {
    if n == 0 || t < from || t >= to {
        return None;
    }
    Some(((u128::from(t - from) * n as u128) / u128::from(to - from)) as usize)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::data::logs::scan::Builder;
    use journal::Entry;

    pub fn entry(t_us: u64, prio: u8, source: &str, msg: &str, extra: &[(&str, &str)]) -> Entry {
        let mut e = Entry {
            t_us,
            ..Default::default()
        };
        e.fields.insert("PRIORITY".into(), prio.to_string());
        e.fields.insert("SYSLOG_IDENTIFIER".into(), source.into());
        e.fields.insert("MESSAGE".into(), msg.into());
        e.fields.insert("_BOOT_ID".into(), "a".repeat(32));
        for (k, v) in extra {
            e.fields.insert((*k).into(), (*v).into());
        }
        e
    }

    pub fn kernel(t_us: u64, prio: u8, msg: &str) -> Entry {
        entry(t_us, prio, "kernel", msg, &[("_TRANSPORT", "kernel")])
    }

    pub fn build(entries: &[Entry]) -> Index {
        let mut b = Builder::default();
        for e in entries {
            b.add_entry(e);
        }
        let mut idx = Index::default();
        idx.append(b.take());
        idx
    }

    fn q(s: &str) -> Query {
        Query::parse(s, 0, &jiff::tz::TimeZone::UTC)
    }

    fn rows(m: &Matches) -> Vec<(u32, u32)> {
        m.rows.iter().map(|v| (v.row, v.count)).collect()
    }

    #[test]
    fn a_row_fits_in_32_bytes() {
        assert!(std::mem::size_of::<Row>() <= 32);
    }

    #[test]
    fn folding_joins_only_consecutive_identical_messages_from_one_source() {
        let idx = build(&[
            entry(1, 6, "a", "same", &[]),
            entry(2, 6, "a", "same", &[]),
            entry(3, 6, "b", "same", &[]),
            entry(4, 6, "a", "same", &[]),
            entry(5, 6, "a", "other", &[]),
            entry(6, 6, "a", "same", &[]),
        ]);
        assert_eq!(
            rows(&idx.matching(&q(""), None)),
            [(0, 2), (2, 1), (3, 1), (4, 1), (5, 1)],
            "a different source or an interleaved message breaks the run"
        );
    }

    #[test]
    fn folding_does_not_cross_a_boot_boundary() {
        let mut other = entry(2, 6, "a", "same", &[]);
        other.fields.insert("_BOOT_ID".into(), "b".repeat(32));
        let idx = build(&[entry(1, 6, "a", "same", &[]), other]);
        assert_eq!(rows(&idx.matching(&q(""), None)), [(0, 1), (1, 1)]);
    }

    #[test]
    fn rows_hidden_by_a_filter_do_not_break_a_fold() {
        let idx = build(&[
            entry(1, 3, "a", "same", &[]),
            entry(2, 6, "a", "chatter", &[]),
            entry(3, 3, "a", "same", &[]),
        ]);
        let m = idx.matching(&q("prio:err"), None);
        assert_eq!(
            (rows(&m), m.entries),
            (vec![(0, 2)], 2),
            "folding is over what is shown"
        );
    }

    #[test]
    fn warning_includes_errors_but_not_notices() {
        let idx = build(&[
            kernel(1, 2, "crit"),
            kernel(2, 3, "err"),
            kernel(3, 4, "warn"),
            kernel(4, 5, "notice"),
            kernel(5, 6, "info"),
        ]);
        let ids: Vec<u32> = idx
            .matching(&q("prio:warning"), None)
            .rows
            .iter()
            .map(|v| v.row)
            .collect();
        assert_eq!(ids, [0, 1, 2]);
    }

    #[test]
    fn a_category_count_ignores_its_own_filter_but_not_the_others() {
        let idx = build(&[
            kernel(1, 3, "k1"),
            kernel(2, 6, "k2"),
            entry(3, 3, "svc", "s1", &[("_SYSTEMD_UNIT", "svc.service")]),
            entry(4, 6, "svc", "s2", &[("_SYSTEMD_UNIT", "svc.service")]),
        ]);
        let counts = idx.category_counts(&q("cat:kernel prio:err"), None);
        assert_eq!(counts.get(Category::Kernel), 1);
        assert_eq!(
            counts.get(Category::Services),
            1,
            "Services is counted as if chosen instead of Kernel, so clicking it shows 1 error"
        );
        assert_eq!(counts.all, 2, "the priority filter still applies");
        assert_eq!(idx.matching(&q("cat:services prio:err"), None).entries, 1);
    }

    #[test]
    fn units_sources_and_boots_narrow_the_rows() {
        let mut older = entry(1, 6, "old", "m", &[("_SYSTEMD_UNIT", "old.service")]);
        older.fields.insert("_BOOT_ID".into(), "b".repeat(32));
        let mut idx = build(&[
            older,
            entry(2, 6, "ssh-ident", "m", &[("_SYSTEMD_UNIT", "sshd.service")]),
            entry(3, 6, "x", "m", &[]),
        ]);
        idx.begin(
            vec![
                Boot {
                    id: "b".repeat(32),
                    first_us: 0,
                    last_us: 1,
                },
                Boot {
                    id: "a".repeat(32),
                    first_us: 2,
                    last_us: 3,
                },
            ],
            "a".repeat(32),
            Coverage::default(),
        );
        let ids = |s: &str| idx.matching(&q(s), None).rows.iter().map(|v| v.row).collect::<Vec<_>>();
        assert_eq!(ids("unit:sshd.service"), [1]);
        assert_eq!(ids("unit:sshd"), [1], "the .service suffix is optional");
        assert_eq!(ids("source:ssh-ident"), [1]);
        assert_eq!(
            ids("unit:nosuch"),
            Vec::<u32>::new(),
            "an unknown unit matches nothing, not everything"
        );
        assert_eq!(ids("boot:0"), [1, 2]);
        assert_eq!(ids("boot:-1"), [0]);
        assert_eq!(ids("boot:-5"), Vec::<u32>::new());
    }

    #[test]
    fn time_ranges_and_search_hits_narrow_the_rows() {
        let idx = build(&[kernel(10, 6, "a"), kernel(20, 6, "b"), kernel(30, 6, "c")]);
        let mut query = q("");
        query.since_us = Some(15);
        query.until_us = Some(25);
        assert_eq!(rows(&idx.matching(&query, None)), [(1, 1)]);
        assert_eq!(rows(&idx.matching(&q(""), Some(&[0, 2]))), [(0, 1), (2, 1)]);
    }

    #[test]
    fn a_finding_shows_only_its_entries() {
        let mut idx = build(&[kernel(1, 6, "a"), kernel(2, 6, "b"), kernel(3, 6, "c")]);
        let mut f = Finding {
            kind: FindingKind::ThermalThrottle,
            severity: journal::findings::Severity::Low,
            first_us: 1,
            last_us: 3,
            title: String::new(),
            evidence: String::new(),
            count: 2,
            entries: vec![2, 0],
            boot_id: String::new(),
            data: Default::default(),
        };
        idx.set_findings(vec![f.clone()]);
        let mut query = q("");
        query.finding = Some(0);
        assert_eq!(rows(&idx.matching(&query, None)), [(0, 1), (2, 1)]);
        f.entries.clear();
        query.finding = Some(7);
        assert!(
            idx.matching(&query, None).rows.is_empty(),
            "a stale finding index shows nothing"
        );
    }

    #[test]
    fn the_histogram_bands_priorities_and_ignores_the_querys_own_time_range() {
        let idx = build(&[
            kernel(0, 2, "e"),
            kernel(1, 4, "w"),
            kernel(5, 5, "n"),
            kernel(6, 6, "i"),
            kernel(9, 7, "d"),
            kernel(10, 3, "outside"),
        ]);
        let mut query = q("");
        query.since_us = Some(8);
        let h = idx.histogram(&query, None, 0, 10, 2);
        assert_eq!(
            h[0],
            Bucket {
                errors: 1,
                warnings: 1,
                notices: 0,
                info: 0
            }
        );
        assert_eq!(
            h[1],
            Bucket {
                errors: 0,
                warnings: 0,
                notices: 1,
                info: 2
            }
        );
    }

    #[test]
    fn lanes_count_a_row_in_every_category_it_belongs_to() {
        let idx = build(&[
            kernel(1, 6, "Linux version 6.1"),
            kernel(2, 6, "plain"),
            entry(3, 6, "s", "m", &[("_SYSTEMD_UNIT", "s.service")]),
        ]);
        let lanes = idx.lanes(&q("cat:services"), None, 0, 4, 2);
        let lane = |c| lanes.iter().find(|l| l.category == c).unwrap().counts.clone();
        assert_eq!(
            lane(Category::Kernel),
            [1, 1],
            "the category filter does not hide other lanes"
        );
        assert_eq!(lane(Category::BootPower), [1, 0]);
        assert_eq!(lane(Category::Services), [0, 1]);
    }

    #[test]
    fn equal_timestamps_are_told_apart_by_their_position_among_equal_hashes() {
        let idx = build(&[
            entry(5, 6, "a", "x", &[]),
            entry(5, 6, "a", "y", &[]),
            entry(5, 6, "a", "x", &[]),
            entry(6, 6, "a", "x", &[]),
        ]);
        assert_eq!([0, 1, 2, 3].map(|i| idx.occurrence(i)), [0, 0, 1, 0]);
    }
}
