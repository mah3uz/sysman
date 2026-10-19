use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use journal::categories::Category;
use journal::findings::{Finding, FindingKind};
use journal::packages::{Action, Transaction};
use serde::{Deserialize, Serialize};

use crate::charts::format;
use crate::data::logs::{BootEnd, BootSummary, Query, Scope, Visible};
use crate::ui::widgets::range::Range;

pub const SAVED_KEY: &str = "logs.saved";
pub const FINDINGS_KEY: &str = "logs.findings";
pub const LANES_KEY: &str = "logs.lanes";

/// What the scan covers, as the range control and the boot list pick it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    Last(Range),
    ThisBoot,
    /// A finished boot by its id; the running one is [`Span::ThisBoot`].
    Boot(String),
}

impl Span {
    pub fn scope(&self, now: u64) -> Scope {
        match self {
            Span::Last(r) => Scope::since(now.saturating_sub(r.span_s() as u64 * 1_000_000)),
            Span::ThisBoot => Scope::this_boot(),
            Span::Boot(id) => Scope::boot(id),
        }
    }

    /// Following only means something for a scope that runs to the present.
    pub fn reaches_now(&self) -> bool {
        !matches!(self, Span::Boot(_))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Level {
    #[default]
    All,
    Notice,
    Warning,
    Errors,
}

impl Level {
    pub const ALL: [(Level, &'static str); 4] = [
        (Level::All, "All"),
        (Level::Notice, "Notice+"),
        (Level::Warning, "Warning+"),
        (Level::Errors, "Errors"),
    ];

    fn max_priority(self) -> Option<u8> {
        match self {
            Level::All => None,
            Level::Notice => Some(5),
            Level::Warning => Some(4),
            Level::Errors => Some(3),
        }
    }
}

/// The controls around the search field, layered over what the field's text parsed to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Controls {
    pub level: Level,
    pub category: Option<Category>,
    /// The findings of the card the list is narrowed to.
    pub finding: Option<Vec<usize>>,
    /// The strip's dragged-out slice.
    pub window: Option<(u64, u64)>,
}

impl Controls {
    /// Each control narrows what the text asked for and never widens it.
    pub fn apply(&self, mut q: Query) -> Query {
        if let Some(p) = self.level.max_priority() {
            q.max_priority = Some(q.max_priority.map_or(p, |m| m.min(p)));
        }
        if let Some(c) = self.category {
            q.categories = vec![c];
        }
        q.findings = self.finding.clone();
        if let Some((from, to)) = self.window {
            q.since_us = Some(q.since_us.map_or(from, |s| s.max(from)));
            q.until_us = Some(q.until_us.map_or(to, |u| u.min(to)));
        }
        q
    }
}

/// One line of the log list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Entry(Visible),
    /// Where a finished boot's entries stop, by its place in the boot summaries.
    BootEnd(usize),
}

/// The visible rows with a divider after the last row of each finished boot. `boot_of` gives a row's boot
/// summary; `finished` whether that boot has ended.
pub fn items(rows: &[Visible], boot_of: impl Fn(u32) -> Option<usize>, finished: impl Fn(usize) -> bool) -> Vec<Item> {
    let mut out = Vec::with_capacity(rows.len() + 4);
    let mut ended = HashSet::new();
    let boots: Vec<Option<usize>> = rows.iter().map(|v| boot_of(v.row)).collect();
    for (i, v) in rows.iter().enumerate() {
        out.push(Item::Entry(*v));
        let Some(b) = boots[i] else { continue };
        let next = boots.get(i + 1).copied().flatten();
        // package rows can sit apart from their boot's run, so a boot ends once, at its first change
        if next != Some(b) && finished(b) && ended.insert(b) {
            out.push(Item::BootEnd(b));
        }
    }
    out
}

/// "Current boot", "Boot −1", … counted back from the running boot. `boots` is oldest first.
pub fn boot_label(i: usize, boots: &[BootSummary]) -> String {
    let current = boots
        .iter()
        .position(|b| b.current)
        .unwrap_or(boots.len().saturating_sub(1));
    match current.checked_sub(i) {
        Some(0) if boots.get(i).is_some_and(|b| b.current) => "Current boot".into(),
        Some(n) => format!("Boot −{n}"),
        None => format!("Boot +{}", i - current),
    }
}

/// The time between one boot's last entry and the next one's first. After an end without shutdown the machine may
/// have hung with the power on, so that gap is not called time off.
pub fn off_time(older: &BootSummary, newer: &BootSummary) -> String {
    match older.end {
        BootEnd::Abrupt => format!("Ended without shutdown · {}", next_boot(older, newer)),
        _ => format!("Off for {}", span(newer.first_us.saturating_sub(older.last_us))),
    }
}

pub fn next_boot(older: &BootSummary, newer: &BootSummary) -> String {
    format!("next boot {} later", span(newer.first_us.saturating_sub(older.last_us)))
}

/// A duration in microseconds, in seconds while under two minutes.
pub fn span(us: u64) -> String {
    let s = us / 1_000_000;
    if s < 120 { format!("{s} s") } else { format::duration(s) }
}

/// A boot that ended without shutdown can explain everything after it, so it leads; the nagging kinds come last.
fn rank(kind: FindingKind) -> u8 {
    match kind {
        FindingKind::BootWithoutShutdown => 0,
        FindingKind::OomKill => 1,
        FindingKind::HardwareError => 2,
        FindingKind::Crash => 3,
        FindingKind::RestartLoop => 4,
        FindingKind::FailedUnit => 5,
        FindingKind::FailedSignIns => 6,
        FindingKind::ThermalThrottle => 7,
        FindingKind::PackageChange => 8,
    }
}

/// Indexes into `findings` by [`rank`], the latest first within a rank.
pub fn ordered(findings: &[Finding]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..findings.len()).collect();
    order.sort_by(|&a, &b| {
        let (a, b) = (&findings[a], &findings[b]);
        rank(a.kind).cmp(&rank(b.kind)).then(b.last_us.cmp(&a.last_us))
    });
    order
}

/// What findings of one kind must share to fold into one card; `None` keeps a finding on its own.
fn subject(f: &Finding) -> Option<String> {
    let get = |k: &str| f.data.get(k).filter(|v| !v.is_empty()).cloned();
    match f.kind {
        FindingKind::Crash => Some(format!("{}\0{}", get("comm")?, get("signal").unwrap_or_default())),
        FindingKind::FailedUnit | FindingKind::RestartLoop => get("unit"),
        FindingKind::OomKill => get("comm"),
        _ => None,
    }
}

/// A unit's failure with result `core-dump` this close to a crash of its process is that same crash.
const CRASH_FAILURE_US: u64 = 10_000_000;

/// A crash in a user service names only the user manager as its unit, so there the program's name must appear in
/// the failed unit's.
fn same_crash(failed: &Finding, crash: &Finding) -> bool {
    let get = |f: &Finding, k: &str| f.data.get(k).filter(|v| !v.is_empty()).map(|v| v.to_lowercase());
    let (Some(unit), Some(crashed_in)) = (get(failed, "unit"), get(crash, "unit")) else {
        return false;
    };
    let ours = unit == crashed_in
        || (crashed_in.starts_with("user@") && get(crash, "comm").is_some_and(|c| unit.contains(&c)));
    failed.kind == FindingKind::FailedUnit
        && crash.kind == FindingKind::Crash
        && failed.count <= 1
        && failed.data.get("result").map(String::as_str) == Some("core-dump")
        && failed.boot_id == crash.boot_id
        && ours
        && failed.first_us.abs_diff(crash.first_us) <= CRASH_FAILURE_US
}

/// The cards: [`ordered`] findings with the same kind and [`subject`] folded into the first of them, each
/// card's members newest first. A unit failure that is a crash's own goes into that crash's card.
pub fn grouped(findings: &[Finding]) -> Vec<Vec<usize>> {
    let mut out: Vec<Vec<usize>> = vec![];
    let mut by_subject: HashMap<(FindingKind, String), usize> = HashMap::new();
    for i in ordered(findings) {
        if let Some(card) = out
            .iter_mut()
            .find(|c| c.iter().any(|&m| same_crash(&findings[i], &findings[m])))
        {
            card.push(i);
            continue;
        }
        match subject(&findings[i]) {
            Some(s) => match by_subject.entry((findings[i].kind, s)) {
                Entry::Occupied(e) => out[*e.get()].push(i),
                Entry::Vacant(e) => {
                    e.insert(out.len());
                    out.push(vec![i]);
                }
            },
            None => out.push(vec![i]),
        }
    }
    out
}

/// The title of a card folding `members` findings like `f` that hold `events` between them.
pub fn folded_title(f: &Finding, members: usize, events: usize) -> String {
    let get = |k: &str| f.data.get(k).map_or("", String::as_str);
    match f.kind {
        FindingKind::Crash => {
            let signal = f.data.get("signal").map_or("unknown signal", String::as_str);
            format!("{} crashed {events} times ({signal})", get("comm"))
        }
        FindingKind::OomKill => format!("{} was killed by the OOM killer {events} times", get("comm")),
        FindingKind::FailedUnit => format!("{} failed {events} times ({})", get("unit"), get("result")),
        FindingKind::RestartLoop => format!("{} kept restarting, {members} times ({events} restarts)", get("unit")),
        _ => f.title.clone(),
    }
}

/// How many of each kind, most severe first: crashes, OOM kills and failed units by event (as the boot tallies
/// count them), the rest by finding. The third number is how many units the failures are of.
pub fn tallies(findings: &[Finding]) -> Vec<(FindingKind, usize, usize)> {
    let mut out: Vec<(FindingKind, usize, usize)> = vec![];
    let mut units: HashSet<&str> = HashSet::new();
    for i in ordered(findings) {
        let f = &findings[i];
        let n = match f.kind {
            FindingKind::Crash | FindingKind::OomKill | FindingKind::FailedUnit => f.count.max(1),
            _ => 1,
        };
        if f.kind == FindingKind::FailedUnit {
            units.insert(f.data.get("unit").map_or("", String::as_str));
        }
        match out.iter_mut().find(|(k, _, _)| *k == f.kind) {
            Some((_, total, _)) => *total += n,
            None => out.push((f.kind, n, 0)),
        }
    }
    for (k, _, u) in &mut out {
        if *k == FindingKind::FailedUnit {
            *u = units.len();
        }
    }
    out
}

/// `units` matters only for failures: "63 unit failures (11 units)" when they are not one each.
pub fn tally_label(kind: FindingKind, n: usize, units: usize) -> String {
    if kind == FindingKind::FailedUnit && units > 1 && units < n {
        return format!("{n} unit failures ({units} units)");
    }
    let (one, many) = match kind {
        FindingKind::Crash => ("crash", "crashes"),
        FindingKind::OomKill => ("OOM kill", "OOM kills"),
        FindingKind::FailedUnit if units == 1 && n > 1 => ("unit failure", "failures of one unit"),
        FindingKind::FailedUnit => ("unit failure", "unit failures"),
        FindingKind::RestartLoop => ("restart loop", "restart loops"),
        FindingKind::BootWithoutShutdown if n == 1 => return "Ended without shutdown".into(),
        FindingKind::BootWithoutShutdown => ("boot without shutdown", "boots without shutdown"),
        FindingKind::ThermalThrottle => ("throttling report", "throttling reports"),
        FindingKind::HardwareError => ("hardware error report", "hardware error reports"),
        FindingKind::FailedSignIns => ("failed sign-in report", "failed sign-in reports"),
        FindingKind::PackageChange => ("package change", "package changes"),
    };
    format!("{n} {}", if n == 1 { one } else { many })
}

pub fn category_label(c: Category) -> &'static str {
    match c {
        Category::SignIns => "Sign-ins & privileges",
        c => c.label(),
    }
}

/// One line for a package transaction's row.
pub fn transaction_line(t: &Transaction) -> String {
    const SHOWN: usize = 3;
    let verb = |a: Action| match a {
        Action::Installed => "installed",
        Action::Upgraded => "upgraded",
        Action::Downgraded => "downgraded",
        Action::Removed => "removed",
        Action::Reinstalled => "reinstalled",
        Action::Purged => "purged",
    };
    let mut parts: Vec<String> = t
        .changes
        .iter()
        .take(SHOWN)
        .map(|c| match (&c.old, &c.new) {
            (Some(old), Some(new)) if old != new => format!("{} {} ({old} → {new})", verb(c.action), c.name),
            (_, Some(v)) | (Some(v), None) => format!("{} {} ({v})", verb(c.action), c.name),
            (None, None) => format!("{} {}", verb(c.action), c.name),
        })
        .collect();
    if t.changes.len() > SHOWN {
        parts.push(format!("{} more", t.changes.len() - SHOWN));
    }
    let mut line = parts.join(", ");
    if let Some(first) = line.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    if line.is_empty() {
        line = t.command.clone().unwrap_or_else(|| "Package transaction".into());
    }
    line
}

/// The message without terminal colour codes, which some programs write into the journal.
pub fn plain(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut chars = message.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while chars.next().is_some_and(|c| !c.is_ascii_alphabetic()) {}
            }
            continue;
        }
        out.push(c);
    }
    out
}

pub fn clock(us: u64, pattern: &str, tz: &TimeZone) -> String {
    Timestamp::from_microsecond(us as i64)
        .map(|t| t.to_zoned(tz.clone()).strftime(pattern).to_string())
        .unwrap_or_default()
}

/// Whether two instants fall on the same local day.
pub fn same_day(a: u64, b: u64, tz: &TimeZone) -> bool {
    clock(a, "%F", tz) == clock(b, "%F", tz)
}

/// Axis labels for `from..to`: about six, on round times, as fractions of the span.
pub fn ticks(from: u64, to: u64, tz: &TimeZone) -> Vec<(f32, String)> {
    const MIN: u64 = 60_000_000;
    const STEPS: [u64; 12] = [
        MIN,
        5 * MIN,
        10 * MIN,
        15 * MIN,
        30 * MIN,
        60 * MIN,
        120 * MIN,
        180 * MIN,
        360 * MIN,
        720 * MIN,
        1440 * MIN,
        2880 * MIN,
    ];
    if to <= from {
        return vec![];
    }
    let len = to - from;
    let step = STEPS.into_iter().find(|s| len / s <= 7).unwrap_or(STEPS[11]);
    let days = step >= 1440 * MIN;
    // round times are counted in local time, so a day tick sits on midnight rather than on UTC's
    let offset = Timestamp::from_microsecond(from as i64)
        .map(|t| tz.to_offset(t).seconds() as i64 * 1_000_000)
        .unwrap_or(0);
    let local = from as i64 + offset;
    let first = (local.div_euclid(step as i64) + 1) * step as i64 - offset;
    let pattern = if days { "%b %-d" } else { "%H:%M" };
    (0..)
        .map(|i| first as u64 + i * step)
        .take_while(|&t| t < to)
        .map(|t| ((t - from) as f32 / len as f32, clock(t, pattern, tz)))
        .collect()
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A saved search: what the field held, under a name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFilter {
    pub name: String,
    pub query: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub filters: Vec<SavedFilter>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingsPref {
    pub collapsed: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanesPref {
    pub open: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::prefs::Prefs;

    fn boot(first_s: u64, last_s: u64, current: bool, end: BootEnd) -> BootSummary {
        BootSummary {
            id: format!("{first_s}"),
            first_us: first_s * 1_000_000,
            last_us: last_s * 1_000_000,
            current,
            end,
            crashes: 0,
            failed_units: 0,
            oom_kills: 0,
            note: None,
        }
    }

    fn row(id: u32) -> Visible {
        Visible {
            row: id,
            last: id,
            count: 1,
        }
    }

    #[test]
    fn the_controls_only_ever_narrow_the_typed_query() {
        let typed = Query {
            max_priority: Some(3),
            since_us: Some(100),
            ..Query::default()
        };
        let controls = Controls {
            level: Level::Warning,
            window: Some((50, 200)),
            ..Controls::default()
        };
        let q = controls.apply(typed);
        assert_eq!(q.max_priority, Some(3), "prio:err stays stricter than Warning+");
        assert_eq!(
            (q.since_us, q.until_us),
            (Some(100), Some(200)),
            "the slice intersects since:"
        );
        let q = Controls {
            level: Level::Notice,
            ..Controls::default()
        }
        .apply(Query::default());
        assert_eq!(q.max_priority, Some(5));
    }

    #[test]
    fn a_finished_boot_gets_one_divider_after_its_last_row_and_the_running_one_none() {
        // rows 0-1 in boot 0 (finished), 2-3 in boot 1 (running), then a package row filed under boot 0
        let rows = [row(0), row(1), row(2), row(3), row(4)];
        let boot_of = |r: u32| Some(if (2..4).contains(&r) { 1 } else { 0 });
        let shown = items(&rows, boot_of, |b| b == 0);
        assert_eq!(
            shown,
            vec![
                Item::Entry(row(0)),
                Item::Entry(row(1)),
                Item::BootEnd(0),
                Item::Entry(row(2)),
                Item::Entry(row(3)),
                Item::Entry(row(4)),
            ]
        );
        let past = items(&[row(7), row(8)], |_| Some(0), |_| true);
        assert_eq!(
            past.last(),
            Some(&Item::BootEnd(0)),
            "a past boot read alone still says how it ended"
        );
    }

    #[test]
    fn boots_are_named_back_from_the_running_one() {
        let boots = [
            boot(0, 10, false, BootEnd::Clean),
            boot(20, 30, false, BootEnd::Abrupt),
            boot(40, 50, true, BootEnd::Running),
        ];
        assert_eq!(boot_label(2, &boots), "Current boot");
        assert_eq!(boot_label(1, &boots), "Boot −1");
        assert_eq!(boot_label(0, &boots), "Boot −2");
    }

    #[test]
    fn the_gap_after_a_boot_without_shutdown_does_not_claim_the_machine_was_off() {
        let lost = boot(0, 100, false, BootEnd::Abrupt);
        let next = boot(193, 300, true, BootEnd::Running);
        assert_eq!(
            off_time(&lost, &next),
            "Ended without shutdown · next boot 93 s later",
            "a hang before a reset leaves the same gap with the power on"
        );
        let clean = boot(0, 100, false, BootEnd::Clean);
        let morning = boot(100 + 9 * 3600 + 360, 99_999, true, BootEnd::Running);
        assert_eq!(off_time(&clean, &morning), "Off for 9h 6m");
    }

    fn finding(kind: FindingKind, last_us: u64, count: usize) -> Finding {
        Finding {
            kind,
            severity: kind.severity(),
            first_us: last_us,
            last_us,
            title: String::new(),
            evidence: String::new(),
            count,
            entries: vec![],
            boot_id: String::new(),
            cursor: None,
            data: Default::default(),
        }
    }

    fn about(kind: FindingKind, last_us: u64, data: &[(&str, &str)]) -> Finding {
        let mut f = finding(kind, last_us, 1);
        f.data = data.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        f
    }

    fn crash(comm: &str, signal: &str, last_us: u64) -> Finding {
        about(FindingKind::Crash, last_us, &[("comm", comm), ("signal", signal)])
    }

    #[test]
    fn findings_come_by_rank_then_latest() {
        let f = [
            finding(FindingKind::PackageChange, 900, 1),
            finding(FindingKind::Crash, 100, 1),
            finding(FindingKind::RestartLoop, 500, 3),
            finding(FindingKind::OomKill, 300, 1),
            finding(FindingKind::Crash, 200, 1),
            finding(FindingKind::ThermalThrottle, 950, 1),
        ];
        assert_eq!(ordered(&f), vec![3, 4, 1, 2, 5, 0]);
    }

    #[test]
    fn a_power_loss_outranks_any_number_of_crashes() {
        let mut f: Vec<Finding> = (0..20)
            .map(|i| crash(&format!("app{i}"), "SIGSEGV", 1000 + i))
            .collect();
        f.push(finding(FindingKind::BootWithoutShutdown, 10, 1));
        assert_eq!(
            grouped(&f)[0],
            vec![20],
            "the cut explains the crashes after it, so it is seen first"
        );
    }

    #[test]
    fn crashes_of_one_program_fold_into_one_card() {
        let f = [
            crash("Hyprland", "SIGSEGV", 100),
            crash("fladder", "SIGABRT", 150),
            crash("Hyprland", "SIGSEGV", 300),
            crash("Hyprland", "SIGABRT", 200),
            crash("Hyprland", "SIGSEGV", 50),
        ];
        let cards = grouped(&f);
        assert_eq!(
            cards,
            vec![vec![2, 0, 4], vec![3], vec![1]],
            "newest first, a different signal stands apart"
        );
        assert_eq!(folded_title(&f[2], 3, 3), "Hyprland crashed 3 times (SIGSEGV)");
    }

    #[test]
    fn units_and_oom_victims_fold_by_name() {
        let f = [
            about(
                FindingKind::FailedUnit,
                100,
                &[("unit", "a.service"), ("result", "exit-code")],
            ),
            about(FindingKind::FailedUnit, 200, &[("unit", "b.service")]),
            about(
                FindingKind::FailedUnit,
                300,
                &[("unit", "a.service"), ("result", "exit-code")],
            ),
            about(FindingKind::OomKill, 400, &[("comm", "firefox")]),
            about(FindingKind::OomKill, 500, &[("comm", "firefox")]),
            about(FindingKind::OomKill, 600, &[]),
            about(FindingKind::OomKill, 700, &[]),
        ];
        assert_eq!(
            grouped(&f),
            vec![vec![6], vec![5], vec![4, 3], vec![2, 0], vec![1]],
            "kills with no victim named are not taken for one process"
        );
        assert_eq!(folded_title(&f[2], 2, 2), "a.service failed 2 times (exit-code)");
    }

    #[test]
    fn boot_ends_are_never_folded() {
        let f = [
            about(FindingKind::BootWithoutShutdown, 100, &[("boot_started_us", "1")]),
            about(FindingKind::BootWithoutShutdown, 200, &[("boot_started_us", "1")]),
        ];
        assert_eq!(
            grouped(&f),
            vec![vec![1], vec![0]],
            "each lost boot has its own last readings"
        );
    }

    #[test]
    fn crashes_are_tallied_by_event_and_loops_by_finding() {
        let f = [
            finding(FindingKind::Crash, 100, 2),
            finding(FindingKind::Crash, 200, 1),
            finding(FindingKind::RestartLoop, 300, 5),
        ];
        let t = tallies(&f);
        assert_eq!(t, vec![(FindingKind::Crash, 3, 0), (FindingKind::RestartLoop, 1, 0)]);
        assert_eq!(tally_label(FindingKind::Crash, 3, 0), "3 crashes");
        assert_eq!(tally_label(FindingKind::RestartLoop, 1, 0), "1 restart loop");
    }

    #[test]
    fn failures_are_counted_as_failures_not_as_failed_units() {
        let mut f: Vec<Finding> = (0..3)
            .map(|i| {
                let mut f = about(FindingKind::FailedUnit, 100 + i, &[("unit", "pcr.service")]);
                f.count = 9;
                f
            })
            .collect();
        f.push(about(FindingKind::FailedUnit, 500, &[("unit", "keyd.service")]));
        let t = tallies(&f);
        assert_eq!(t, vec![(FindingKind::FailedUnit, 28, 2)]);
        assert_eq!(
            tally_label(FindingKind::FailedUnit, 28, 2),
            "28 unit failures (2 units)",
            "one unit failing every boot is not 28 failed units"
        );
        assert_eq!(tally_label(FindingKind::FailedUnit, 1, 1), "1 unit failure");
        assert_eq!(tally_label(FindingKind::FailedUnit, 4, 1), "4 failures of one unit");
        assert_eq!(tally_label(FindingKind::FailedUnit, 2, 2), "2 unit failures");
    }

    #[test]
    fn a_service_crash_and_the_failure_it_caused_are_one_card() {
        let mut crash = about(
            FindingKind::Crash,
            1_000_000_000,
            &[
                ("comm", "nautilus"),
                ("signal", "SIGSEGV"),
                ("unit", "nautilus.service"),
            ],
        );
        crash.first_us = crash.last_us;
        let failed = |at: u64, unit: &str, result: &str| {
            about(FindingKind::FailedUnit, at, &[("unit", unit), ("result", result)])
        };
        let f = [
            crash,
            failed(1_002_000_000, "nautilus.service", "core-dump"),
            failed(1_002_000_000, "other.service", "core-dump"),
            failed(1_002_000_000, "nautilus.service", "exit-code"),
            failed(1_900_000_000, "nautilus.service", "core-dump"),
        ];
        assert_eq!(
            grouped(&f),
            vec![vec![0, 1], vec![4, 3], vec![2]],
            "the unit failed because its process dumped core: one event, seen once; a later failure, another \
             result or another unit stays a failure"
        );
        let in_user_manager = about(
            FindingKind::Crash,
            1_000_000_000,
            &[("comm", "nautilus"), ("unit", "user@1000.service")],
        );
        let f = [
            in_user_manager,
            failed(1_000_045_000, "dbus-:1.2-org.gnome.Nautilus@0.service", "core-dump"),
            failed(1_000_045_000, "pipewire.service", "core-dump"),
        ];
        assert_eq!(
            grouped(&f),
            vec![vec![0, 1], vec![2]],
            "a user service's crash names the user manager, so the program's name ties it to its unit"
        );
    }

    #[test]
    fn axis_ticks_land_on_round_local_times() {
        let tz = TimeZone::fixed(jiff::tz::offset(6));
        let from = 1_760_000_000_000_000; // 08:53:20 UTC, 14:53:20 at +06
        let ticks = ticks(from, from + 6 * 3_600_000_000, &tz);
        assert_eq!(
            ticks.iter().map(|(_, l)| l.as_str()).collect::<Vec<_>>(),
            ["15:00", "16:00", "17:00", "18:00", "19:00", "20:00"]
        );
        assert!(ticks.iter().all(|(f, _)| (0. ..1.).contains(f)));
    }

    #[test]
    fn a_package_row_reads_as_a_sentence() {
        use journal::packages::{Change, Manager};
        let change = |name: &str, old: Option<&str>, new: Option<&str>, action| Change {
            action,
            name: name.into(),
            old: old.map(Into::into),
            new: new.map(Into::into),
        };
        let t = Transaction {
            manager: Manager::Pacman,
            start_us: 0,
            end_us: 0,
            command: None,
            requested_by: None,
            changes: vec![
                change("linux", Some("6.11.1"), Some("6.11.2"), Action::Upgraded),
                change("htop", None, Some("3.4"), Action::Installed),
                change("nano", Some("8.0"), None, Action::Removed),
                change("mesa", Some("24.2"), Some("24.3"), Action::Upgraded),
            ],
            warnings: vec![],
        };
        assert_eq!(
            transaction_line(&t),
            "Upgraded linux (6.11.1 → 6.11.2), installed htop (3.4), removed nano (8.0), 1 more"
        );
    }

    #[test]
    fn colour_codes_are_dropped_from_messages() {
        assert_eq!(plain("\u{1b}[1;32mDEBUG \u{1b}[0m]: ready"), "DEBUG ]: ready");
        assert_eq!(plain("no codes"), "no codes");
    }

    #[test]
    fn saved_filters_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        let saved = Saved {
            filters: vec![SavedFilter {
                name: "Errors this boot".into(),
                query: "prio:err boot:0".into(),
            }],
        };
        Prefs::load(path.clone()).set(SAVED_KEY, &saved);
        assert_eq!(Prefs::load(path).get::<Saved>(SAVED_KEY), Some(saved));
    }
}
