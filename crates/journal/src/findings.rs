//! Rules over the journal that turn entries into findings, evaluated in one streaming pass.

use crate::categories::{ids, source};
use crate::entry::Entry;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Write;

const SECOND_US: u64 = 1_000_000;
const MAX_ENTRIES: usize = 200;
const RESTART_WINDOW_US: u64 = 5 * 60 * SECOND_US;
const RESTART_LOOP_MIN: usize = 3;
const USB_WINDOW_US: u64 = 60 * SECOND_US;
const USB_STORM_MIN: usize = 5;
const SIGNIN_WINDOW_US: u64 = 10 * 60 * SECOND_US;
const SIGNIN_FAILURES_MIN: usize = 3;
const OOM_MERGE_US: u64 = 5 * SECOND_US;
const EVIDENCE_CHARS: usize = 200;

// Every text pattern the rules match on, kernel unless noted.
const OOM_KILL: &str = "ut of memory: Kill";
const OOM_KILL_SUMMARY: &str = "oom-kill:constraints=";
const THROTTLE: &[&str] = &["clock throttled", "temperature above threshold"];
const USB_DISCONNECT: &str = ": USB disconnect";
const HARDWARE: &[(&str, &[&str])] = &[
    ("PCIe AER errors", &["AER:"]),
    (
        "Machine check events",
        &["[Hardware Error]", "Machine check", "Machine Check"],
    ),
    ("Memory (EDAC) errors", &["EDAC MC"]),
    ("Disk I/O errors", &["I/O error, dev ", "Buffer I/O error"]),
];
const EDAC_ERROR: &[&str] = &[" CE ", " UE ", "error"];
const SIGNIN_FAILURE: &[&str] = &["Failed password", "authentication failure"];
const AUTH_FACILITIES: &[&str] = &["4", "10"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Low,
    Medium,
    High,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Low => "Low",
            Severity::Medium => "Medium",
            Severity::High => "High",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FindingKind {
    Crash,
    OomKill,
    FailedUnit,
    RestartLoop,
    BootWithoutShutdown,
    ThermalThrottle,
    HardwareError,
    FailedSignIns,
    /// Produced from the package history, never by a journal rule.
    PackageChange,
}

impl FindingKind {
    pub fn severity(self) -> Severity {
        match self {
            FindingKind::Crash
            | FindingKind::OomKill
            | FindingKind::BootWithoutShutdown
            | FindingKind::HardwareError => Severity::High,
            FindingKind::FailedUnit | FindingKind::RestartLoop | FindingKind::FailedSignIns => Severity::Medium,
            FindingKind::ThermalThrottle | FindingKind::PackageChange => Severity::Low,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            FindingKind::Crash => "crash",
            FindingKind::OomKill => "oom",
            FindingKind::FailedUnit => "failed-unit",
            FindingKind::RestartLoop => "restart-loop",
            FindingKind::BootWithoutShutdown => "abrupt-end",
            FindingKind::ThermalThrottle => "thermal",
            FindingKind::HardwareError => "hardware",
            FindingKind::FailedSignIns => "sign-ins",
            FindingKind::PackageChange => "packages",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub kind: FindingKind,
    pub severity: Severity,
    pub first_us: u64,
    pub last_us: u64,
    pub title: String,
    pub evidence: String,
    pub count: usize,
    /// Caller row indexes of the entries that make it up, at most 200.
    pub entries: Vec<usize>,
    pub boot_id: String,
    pub data: BTreeMap<String, String>,
    /// `__CURSOR` of the first entry, when entries were read with it.
    pub cursor: Option<String>,
}

impl Finding {
    fn new(kind: FindingKind, t_us: u64, index: usize, boot: &str, title: String, evidence: String) -> Finding {
        Finding {
            kind,
            severity: kind.severity(),
            first_us: t_us,
            last_us: t_us,
            title,
            evidence,
            count: 1,
            entries: vec![index],
            boot_id: boot.to_string(),
            data: BTreeMap::new(),
            cursor: None,
        }
    }

    fn with(mut self, key: &str, value: impl Into<String>) -> Finding {
        self.data.insert(key.to_string(), value.into());
        self
    }

    fn add(&mut self, t_us: u64, index: usize) {
        self.last_us = t_us;
        self.count += 1;
        if self.entries.len() < MAX_ENTRIES {
            self.entries.push(index);
        }
        self.retitle();
    }

    fn field(&self, key: &str) -> &str {
        self.data.get(key).map_or("", String::as_str)
    }

    fn retitle(&mut self) {
        let n = self.count;
        let title = match self.kind {
            FindingKind::FailedUnit => {
                let once = format!("{} failed ({})", self.field("unit"), self.field("result"));
                if n > 1 {
                    format!("{once}, {n} times")
                } else {
                    once
                }
            }
            FindingKind::RestartLoop => format!("{} keeps restarting ({n} restarts)", self.field("unit")),
            FindingKind::ThermalThrottle => format!("Thermal throttling ({n} events)"),
            FindingKind::FailedSignIns => format!("Repeated failed sign-ins ({n})"),
            FindingKind::HardwareError if self.data.contains_key("device") => {
                format!("{} disconnected {n} times in a minute", self.field("device"))
            }
            FindingKind::HardwareError => format!("{} ({n})", self.field("error")),
            FindingKind::OomKill => oom_title(self),
            _ => return,
        };
        self.title = title;
    }
}

fn oom_title(f: &Finding) -> String {
    let comm = f.field("comm");
    if !comm.is_empty() {
        return format!("{comm} was killed by the OOM killer");
    }
    match f.field("unit") {
        "" => "A process was killed for lack of memory".to_string(),
        unit => format!("{unit} hit the OOM killer"),
    }
}

/// Two entries from the same source with the same text, for folding repeats in the list.
pub fn same_message(a: &Entry, b: &Entry) -> bool {
    source(a) == source(b) && a.message() == b.message()
}

#[derive(Default)]
struct BootSpan {
    id: String,
    first_us: u64,
    last_us: u64,
    last_index: usize,
    last_line: String,
    stopped_cleanly: bool,
}

type BurstKey = (&'static str, String, String);

struct Hit {
    t: u64,
    index: usize,
    cursor: Option<String>,
}

#[derive(Default)]
struct Burst {
    events: VecDeque<Hit>,
    finding: Option<usize>,
}

/// Feed entries in time order with `push`, then `finish`; or, when following, `drain` as time moves on. Holds
/// per-rule state, never the entries.
#[derive(Default)]
pub struct Detector {
    out: Vec<Finding>,
    /// Each finding's `count` when `drain` last returned it.
    reported: Vec<usize>,
    boots: Vec<BootSpan>,
    current: usize,
    crashes: HashMap<(String, String), usize>,
    failed: HashMap<(String, String), usize>,
    grouped: HashMap<(&'static str, String, String), usize>,
    bursts: HashMap<BurstKey, Burst>,
    last_oom: Option<usize>,
}

impl Detector {
    pub fn new() -> Detector {
        Detector::default()
    }

    pub fn push(&mut self, index: usize, e: &Entry) {
        let boot = e.get("_BOOT_ID").unwrap_or_default();
        self.track_boot(index, e, boot);
        let t = e.t_us;
        let before = self.out.len();
        match e.get("MESSAGE_ID") {
            Some(ids::COREDUMP) => self.crash(index, e, boot),
            Some(ids::UNIT_FAILURE) => self.failed_unit(index, e, boot),
            Some(ids::RESTART_SCHEDULED) => self.restart(index, e, boot),
            Some(ids::OOMD_KILL | ids::UNIT_OOM) => self.oom(index, e, boot),
            Some(ids::SHUTDOWN | ids::JOURNAL_STOPPED) => self.boots[self.current].stopped_cleanly = true,
            _ => {}
        }
        if e.get("_TRANSPORT") == Some("kernel") {
            self.kernel_line(index, e, boot);
        } else if is_auth(e) {
            let message = e.message();
            if SIGNIN_FAILURE.iter().any(|p| message.contains(p)) {
                self.signin_failure(index, t, e, boot);
            }
        }
        // A burst names its own first entry; every other new finding starts at this one.
        for f in &mut self.out[before..] {
            if f.cursor.is_none() {
                f.cursor = e.get("__CURSOR").map(str::to_string);
            }
        }
    }

    /// The findings created or grown since the last drain, oldest first. `now_us` promises that no entry older than
    /// it is still to be pushed, so every rule window that ended before it is closed and its state dropped: a long
    /// follow holds only what can still grow. An open finding is returned again each time it grows, with the same
    /// `first_us` and `cursor`. A unit's failures fold until an entry of another boot arrives. Findings drained and
    /// closed are not returned by `finish`.
    pub fn drain(&mut self, now_us: u64) -> Vec<Finding> {
        let mut fresh: Vec<Finding> = self
            .out
            .iter()
            .enumerate()
            .filter(|(i, f)| self.reported.get(*i) != Some(&f.count))
            .map(|(_, f)| f.clone())
            .collect();
        fresh.sort_by_key(|f| f.first_us);

        let boot = self.boots.get(self.current).map(|b| b.id.clone()).unwrap_or_default();
        let ended = |last: u64, window: u64| now_us.saturating_sub(last) > window;
        self.failed.retain(|k, _| k.0 == boot);
        self.grouped.retain(|k, _| k.1 == boot);
        self.crashes.retain(|k, _| k.0 == boot);
        self.bursts
            .retain(|k, b| b.events.back().is_some_and(|h| !ended(h.t, burst_window(k.0))));
        self.last_oom = self.last_oom.filter(|&i| !ended(self.out[i].last_us, OOM_MERGE_US));

        let mut held = vec![false; self.out.len()];
        for i in self.held() {
            held[i] = true;
        }
        let mut moved = vec![usize::MAX; held.len()];
        for (to, (from, _)) in held.iter().enumerate().filter(|(_, h)| **h).enumerate() {
            moved[from] = to;
        }
        let mut i = 0;
        self.out.retain(|_| {
            i += 1;
            held[i - 1]
        });
        for v in self.failed.values_mut().chain(self.grouped.values_mut()) {
            *v = moved[*v];
        }
        for b in self.bursts.values_mut() {
            b.finding = b.finding.map(|i| moved[i]);
        }
        self.last_oom = self.last_oom.map(|i| moved[i]);
        self.reported = self.out.iter().map(|f| f.count).collect();
        fresh
    }

    /// The earliest time a still-open rule depends on, as of the last `drain`. A new detector fed from it rebuilds the
    /// open findings with the same first entries, so a reader that stops and resumes there neither splits nor
    /// repeats an episode.
    pub fn open_since(&self) -> Option<u64> {
        let quiet = self.bursts.values().filter_map(|b| b.events.front().map(|h| h.t));
        self.held().map(|i| self.out[i].first_us).chain(quiet).min()
    }

    fn held(&self) -> impl Iterator<Item = usize> + '_ {
        let bursts = self.bursts.values().filter_map(|b| b.finding);
        self.failed
            .values()
            .chain(self.grouped.values())
            .copied()
            .chain(bursts)
            .chain(self.last_oom)
    }

    /// `current_boot` is never reported as ended without shutdown, it has not ended yet.
    pub fn finish(mut self, current_boot: Option<&str>) -> Vec<Finding> {
        for span in &self.boots {
            if span.id.is_empty() || span.stopped_cleanly || Some(span.id.as_str()) == current_boot {
                continue;
            }
            let mut f = Finding::new(
                FindingKind::BootWithoutShutdown,
                span.last_us,
                span.last_index,
                &span.id,
                "Boot ended without a shutdown".to_string(),
                format!("Last entry: {}", span.last_line),
            );
            f.data.insert("boot_started_us".to_string(), span.first_us.to_string());
            self.out.push(f);
        }
        self.out.sort_by_key(|f| f.first_us);
        self.out
    }

    fn track_boot(&mut self, index: usize, e: &Entry, boot: &str) {
        if self.boots.get(self.current).is_none_or(|b| b.id != boot) {
            self.current = match self.boots.iter().position(|b| b.id == boot) {
                Some(i) => i,
                None => {
                    self.boots.push(BootSpan {
                        id: boot.to_string(),
                        first_us: e.t_us,
                        ..BootSpan::default()
                    });
                    self.boots.len() - 1
                }
            };
        }
        let span = &mut self.boots[self.current];
        span.last_us = e.t_us;
        span.last_index = index;
        span.last_line.clear();
        let _ = write!(span.last_line, "{}: {}", source(e), evidence(e.message()));
    }

    fn crash(&mut self, index: usize, e: &Entry, boot: &str) {
        let comm = e.get("COREDUMP_COMM").unwrap_or("A process");
        let signal = e.get("COREDUMP_SIGNAL_NAME").unwrap_or("unknown signal");
        let n = self.crashes.entry((boot.to_string(), comm.to_string())).or_default();
        *n += 1;
        let title = match *n {
            1 => format!("{comm} crashed ({signal})"),
            2 => format!("Second {comm} crash this boot ({signal})"),
            3 => format!("Third {comm} crash this boot ({signal})"),
            n => format!("{n}th {comm} crash this boot ({signal})"),
        };
        let mut shown = evidence(e.message());
        if shown.is_empty() {
            // The reader may drop a long multi-line MESSAGE.
            shown = format!(
                "{comm} (pid {}) received {signal}",
                e.get("COREDUMP_PID").unwrap_or("?")
            );
        }
        let mut f = Finding::new(FindingKind::Crash, e.t_us, index, boot, title, shown);
        f.data.insert("comm".to_string(), comm.to_string());
        for (field, key) in [
            ("COREDUMP_PID", "pid"),
            ("COREDUMP_SIGNAL_NAME", "signal"),
            ("COREDUMP_UNIT", "unit"),
        ] {
            if let Some(v) = e.get(field) {
                f.data.insert(key.to_string(), v.to_string());
            }
        }
        self.out.push(f);
    }

    fn failed_unit(&mut self, index: usize, e: &Entry, boot: &str) {
        let Some(unit) = unit_of(e) else { return };
        let result = e.get("UNIT_RESULT").unwrap_or("failed");
        let at = self.failed.get(&(boot.to_string(), unit.to_string())).copied();
        match at {
            Some(i) => {
                let f = &mut self.out[i];
                f.data.insert("result".to_string(), result.to_string());
                f.add(e.t_us, index);
            }
            None => {
                let mut f = Finding::new(
                    FindingKind::FailedUnit,
                    e.t_us,
                    index,
                    boot,
                    String::new(),
                    evidence(e.message()),
                )
                .with("unit", unit)
                .with("result", result);
                f.retitle();
                self.failed.insert((boot.to_string(), unit.to_string()), self.out.len());
                self.out.push(f);
            }
        }
    }

    fn restart(&mut self, index: usize, e: &Entry, boot: &str) {
        let Some(unit) = unit_of(e) else { return };
        let key = ("restart", boot.to_string(), unit.to_string());
        let hit = self.burst(&key, e, index, RESTART_WINDOW_US, RESTART_LOOP_MIN);
        self.finish_burst(key, hit, e.t_us, index, |first| {
            Finding::new(
                FindingKind::RestartLoop,
                first,
                index,
                boot,
                String::new(),
                evidence(e.message()),
            )
            .with("unit", unit)
        });
    }

    fn signin_failure(&mut self, index: usize, t: u64, e: &Entry, boot: &str) {
        let key = ("signin", boot.to_string(), String::new());
        let hit = self.burst(&key, e, index, SIGNIN_WINDOW_US, SIGNIN_FAILURES_MIN);
        let service = pam_service(e.message());
        self.finish_burst(key, hit, t, index, |first| {
            let f = Finding::new(
                FindingKind::FailedSignIns,
                first,
                index,
                boot,
                String::new(),
                evidence(e.message()),
            );
            match service {
                Some(s) => f.with("service", s),
                None => f,
            }
        });
    }

    fn kernel_line(&mut self, index: usize, e: &Entry, boot: &str) {
        let message = e.message();
        if message.contains(OOM_KILL) || message.contains(OOM_KILL_SUMMARY) {
            return self.oom(index, e, boot);
        }
        if THROTTLE.iter().any(|p| message.contains(p)) {
            return self.group(("thermal", boot.to_string(), String::new()), index, e, || {
                Finding::new(
                    FindingKind::ThermalThrottle,
                    e.t_us,
                    index,
                    boot,
                    String::new(),
                    evidence(message),
                )
            });
        }
        if let Some(at) = message.find(USB_DISCONNECT) {
            let device = &message[..at];
            let key = ("usb", boot.to_string(), device.to_string());
            let hit = self.burst(&key, e, index, USB_WINDOW_US, USB_STORM_MIN);
            self.finish_burst(key, hit, e.t_us, index, |first| {
                Finding::new(
                    FindingKind::HardwareError,
                    first,
                    index,
                    boot,
                    String::new(),
                    evidence(message),
                )
                .with("error", "USB disconnect storm")
                .with("device", device)
            });
            return;
        }
        if let Some(what) = hardware_error(message) {
            self.group(("hardware", boot.to_string(), what.to_string()), index, e, || {
                Finding::new(
                    FindingKind::HardwareError,
                    e.t_us,
                    index,
                    boot,
                    String::new(),
                    evidence(message),
                )
                .with("error", what)
            });
        }
    }

    // A kernel kill line and the manager's report of it arrive within seconds of each other.
    fn oom(&mut self, index: usize, e: &Entry, boot: &str) {
        let message = e.message();
        let (pid, comm) = oom_victim(message);
        let mut extra = Vec::new();
        if let Some(p) = pid {
            extra.push(("pid", p));
        }
        if let Some(c) = comm {
            extra.push(("comm", c));
        }
        if let Some(u) = e.get("UNIT") {
            extra.push(("unit", u.to_string()));
        }
        let merge = self.last_oom.filter(|&i| {
            let f = &self.out[i];
            f.boot_id == boot && e.t_us.saturating_sub(f.last_us) <= OOM_MERGE_US
        });
        let i = match merge {
            Some(i) => {
                self.out[i].add(e.t_us, index);
                i
            }
            None => {
                self.out.push(Finding::new(
                    FindingKind::OomKill,
                    e.t_us,
                    index,
                    boot,
                    String::new(),
                    evidence(message),
                ));
                self.out.len() - 1
            }
        };
        let f = &mut self.out[i];
        for (k, v) in extra {
            f.data.entry(k.to_string()).or_insert(v);
        }
        if f.field("comm").is_empty() && e.get("MESSAGE_ID") == Some(ids::OOMD_KILL) {
            f.data
                .entry("killer".to_string())
                .or_insert_with(|| "systemd-oomd".to_string());
        }
        f.retitle();
        self.last_oom = Some(i);
    }

    fn group(&mut self, key: (&'static str, String, String), index: usize, e: &Entry, make: impl FnOnce() -> Finding) {
        match self.grouped.get(&key) {
            Some(&i) => self.out[i].add(e.t_us, index),
            None => {
                let mut f = make();
                f.retitle();
                self.grouped.insert(key, self.out.len());
                self.out.push(f);
            }
        }
    }

    /// Records an event in its sliding window; reports whether it extends an open episode or just
    /// reached the threshold. A gap longer than the window ends the episode.
    fn burst(&mut self, key: &BurstKey, e: &Entry, index: usize, window: u64, min: usize) -> BurstHit {
        let t = e.t_us;
        let b = self.bursts.entry(key.clone()).or_default();
        if b.events.back().is_some_and(|last| t.saturating_sub(last.t) > window) {
            b.events.clear();
            b.finding = None;
        }
        b.events.push_back(Hit {
            t,
            index,
            cursor: e.get("__CURSOR").map(str::to_string),
        });
        while b.events.front().is_some_and(|first| t.saturating_sub(first.t) > window) {
            b.events.pop_front();
        }
        match b.finding {
            Some(i) => BurstHit::Extend(i),
            None if b.events.len() >= min => BurstHit::Open {
                t: b.events[0].t,
                cursor: b.events[0].cursor.clone(),
                entries: b.events.iter().map(|h| h.index).collect(),
            },
            None => BurstHit::Quiet,
        }
    }

    fn finish_burst(&mut self, key: BurstKey, hit: BurstHit, t: u64, index: usize, make: impl FnOnce(u64) -> Finding) {
        match hit {
            BurstHit::Quiet => {}
            BurstHit::Extend(i) => self.out[i].add(t, index),
            BurstHit::Open {
                t: first,
                cursor,
                entries,
            } => {
                let mut f = make(first);
                f.count = entries.len();
                f.entries = entries;
                f.cursor = cursor;
                f.last_us = t;
                f.retitle();
                if let Some(b) = self.bursts.get_mut(&key) {
                    b.finding = Some(self.out.len());
                }
                self.out.push(f);
            }
        }
    }
}

enum BurstHit {
    Quiet,
    Extend(usize),
    Open {
        t: u64,
        cursor: Option<String>,
        entries: Vec<usize>,
    },
}

fn burst_window(rule: &str) -> u64 {
    match rule {
        "restart" => RESTART_WINDOW_US,
        "usb" => USB_WINDOW_US,
        _ => SIGNIN_WINDOW_US,
    }
}

fn evidence(message: &str) -> String {
    let line = message.lines().next().unwrap_or_default();
    line.chars().take(EVIDENCE_CHARS).collect()
}

fn unit_of(e: &Entry) -> Option<&str> {
    e.get("UNIT").or_else(|| e.get("USER_UNIT"))
}

fn is_auth(e: &Entry) -> bool {
    e.get("SYSLOG_FACILITY").is_some_and(|f| AUTH_FACILITIES.contains(&f))
        || e.get("SYSLOG_IDENTIFIER").is_some_and(|i| i.starts_with("sshd"))
}

fn pam_service(message: &str) -> Option<String> {
    let rest = message.strip_prefix("pam_unix(")?;
    Some(rest.split(':').next()?.to_string())
}

fn hardware_error(message: &str) -> Option<&'static str> {
    HARDWARE.iter().find_map(|(what, needles)| {
        let hit = needles.iter().any(|n| message.contains(n));
        let edac_noise = *what == "Memory (EDAC) errors" && !EDAC_ERROR.iter().any(|p| message.contains(p));
        (hit && !edac_noise).then_some(*what)
    })
}

fn oom_victim(message: &str) -> (Option<String>, Option<String>) {
    if let Some(at) = message.find("task=") {
        let rest = &message[at + 5..];
        let comm = rest.split(',').next().map(str::to_string);
        let pid = rest.split("pid=").nth(1).map(digits);
        return (pid.filter(|p| !p.is_empty()), comm.filter(|c| !c.is_empty()));
    }
    if let Some(at) = message.find(OOM_KILL) {
        let rest = message[at..].split("process ").nth(1).unwrap_or_default();
        let pid = digits(rest);
        let comm = rest
            .split_once('(')
            .and_then(|(_, r)| r.split_once(')'))
            .map(|(c, _)| c.to_string());
        return (Some(pid).filter(|p| !p.is_empty()), comm);
    }
    (None, None)
}

fn digits(s: &str) -> String {
    s.chars().take_while(char::is_ascii_digit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BOOT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const S: u64 = SECOND_US;

    fn ev(t_s: u64, boot: &str, fields: &[(&str, &str)]) -> Entry {
        let mut e = Entry {
            t_us: t_s * S,
            fields: BTreeMap::new(),
        };
        e.fields.insert("_BOOT_ID".into(), boot.into());
        for (k, v) in fields {
            e.fields.insert(k.to_string(), v.to_string());
        }
        e
    }

    fn kernel(t_s: u64, boot: &str, message: &str) -> Entry {
        ev(
            t_s,
            boot,
            &[
                ("_TRANSPORT", "kernel"),
                ("SYSLOG_IDENTIFIER", "kernel"),
                ("MESSAGE", message),
            ],
        )
    }

    fn run(entries: &[Entry], current: Option<&str>) -> Vec<Finding> {
        let mut d = Detector::new();
        for (i, e) in entries.iter().enumerate() {
            d.push(i, e);
        }
        d.finish(current)
    }

    fn of(findings: &[Finding], kind: FindingKind) -> Vec<&Finding> {
        findings.iter().filter(|f| f.kind == kind).collect()
    }

    fn coredump(t_s: u64, boot: &str, comm: &str) -> Entry {
        ev(
            t_s,
            boot,
            &[
                ("MESSAGE_ID", ids::COREDUMP),
                ("COREDUMP_COMM", comm),
                ("COREDUMP_PID", "4242"),
                ("COREDUMP_SIGNAL_NAME", "SIGSEGV"),
                (
                    "MESSAGE",
                    "Process 4242 (Hyprland) of user 1000 dumped core.\n\nStack trace of thread 4242:\n#0 0x1 foo",
                ),
            ],
        )
    }

    fn restart(t_s: u64, unit: &str) -> Entry {
        ev(
            t_s,
            BOOT_A,
            &[
                ("MESSAGE_ID", ids::RESTART_SCHEDULED),
                ("UNIT", unit),
                ("MESSAGE", "Scheduled restart job, restart counter is at 1."),
            ],
        )
    }

    fn shutdown(t_s: u64, boot: &str) -> Entry {
        ev(
            t_s,
            boot,
            &[("MESSAGE_ID", ids::SHUTDOWN), ("MESSAGE", "System is powering down.")],
        )
    }

    fn noise(t_s: u64, boot: &str) -> Entry {
        ev(t_s, boot, &[("MESSAGE", "Started something.")])
    }

    #[test]
    fn a_core_dump_is_a_high_severity_crash_with_the_process_named() {
        let f = run(&[coredump(10, BOOT_A, "Hyprland")], Some(BOOT_A));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, FindingKind::Crash);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].title, "Hyprland crashed (SIGSEGV)");
        assert_eq!(f[0].data["pid"], "4242");
        assert_eq!(f[0].data["signal"], "SIGSEGV");
        assert_eq!(f[0].evidence, "Process 4242 (Hyprland) of user 1000 dumped core.");
    }

    #[test]
    fn a_crash_whose_message_was_dropped_still_has_readable_evidence() {
        let mut e = coredump(10, BOOT_A, "Hyprland");
        e.fields.remove("MESSAGE");
        let f = run(&[e], Some(BOOT_A));
        assert_eq!(f[0].evidence, "Hyprland (pid 4242) received SIGSEGV");
    }

    #[test]
    fn a_repeat_crash_in_one_boot_is_called_out_but_a_new_boot_starts_over() {
        let f = run(
            &[
                coredump(10, BOOT_A, "Hyprland"),
                coredump(20, BOOT_A, "Hyprland"),
                coredump(30, BOOT_A, "waybar"),
                coredump(40, BOOT_B, "Hyprland"),
            ],
            Some(BOOT_B),
        );
        let titles: Vec<_> = of(&f, FindingKind::Crash).iter().map(|f| f.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Hyprland crashed (SIGSEGV)",
                "Second Hyprland crash this boot (SIGSEGV)",
                "waybar crashed (SIGSEGV)",
                "Hyprland crashed (SIGSEGV)"
            ]
        );
    }

    #[test]
    fn a_kernel_oom_kill_and_the_units_report_of_it_are_one_finding_naming_the_victim() {
        let f = run(
            &[
                kernel(100, BOOT_A, "oom-kill:constraints=CONSTRAINT_NONE,nodemask=(null),cpuset=/,mems_allowed=0,global_oom,task_memcg=/user.slice,task=firefox,pid=777,uid=1000"),
                kernel(100, BOOT_A, "Out of memory: Killed process 777 (firefox) total-vm:9000kB, anon-rss:8000kB"),
                ev(101, BOOT_A, &[("MESSAGE_ID", ids::UNIT_OOM), ("UNIT", "app.service"), ("MESSAGE", "app.service: A process of this unit has been killed by the OOM killer.")]),
            ],
            Some(BOOT_A),
        );
        let oom = of(&f, FindingKind::OomKill);
        assert_eq!(oom.len(), 1, "one kill, three lines: {f:#?}");
        assert_eq!(oom[0].severity, Severity::High);
        assert_eq!(oom[0].count, 3);
        assert_eq!(oom[0].data["pid"], "777");
        assert_eq!(oom[0].data["comm"], "firefox");
        assert_eq!(oom[0].data["unit"], "app.service");
        assert_eq!(oom[0].title, "firefox was killed by the OOM killer");
    }

    #[test]
    fn kills_minutes_apart_are_separate_and_oomd_alone_is_a_finding() {
        let f = run(
            &[
                kernel(100, BOOT_A, "Out of memory: Killed process 1 (a) total-vm:1kB"),
                kernel(
                    400,
                    BOOT_A,
                    "Memory cgroup out of memory: Killed process 2 (b) total-vm:1kB",
                ),
                ev(
                    900,
                    BOOT_A,
                    &[
                        ("MESSAGE_ID", ids::OOMD_KILL),
                        ("UNIT", "big.slice"),
                        ("MESSAGE", "Killed /big.slice due to memory pressure"),
                    ],
                ),
            ],
            Some(BOOT_A),
        );
        assert_eq!(of(&f, FindingKind::OomKill).len(), 3);
        assert_eq!(f[2].title, "big.slice hit the OOM killer");
        assert_eq!(f[2].data["killer"], "systemd-oomd");
    }

    #[test]
    fn an_ordinary_kernel_line_mentioning_memory_is_not_an_oom_kill() {
        let f = run(
            &[kernel(1, BOOT_A, "Memory: 32000000K/33000000K available")],
            Some(BOOT_A),
        );
        assert!(f.is_empty());
    }

    #[test]
    fn a_unit_failing_repeatedly_in_one_boot_is_one_finding_with_the_latest_result() {
        let fail = |t, result| {
            ev(
                t,
                BOOT_A,
                &[
                    ("MESSAGE_ID", ids::UNIT_FAILURE),
                    ("UNIT", "backup.service"),
                    ("UNIT_RESULT", result),
                    ("MESSAGE", "backup.service: Failed"),
                ],
            )
        };
        let f = run(&[fail(10, "exit-code"), fail(5000, "timeout")], Some(BOOT_A));
        let failed = of(&f, FindingKind::FailedUnit);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].severity, Severity::Medium);
        assert_eq!(failed[0].count, 2);
        assert_eq!(failed[0].data["unit"], "backup.service");
        assert_eq!(failed[0].title, "backup.service failed (timeout), 2 times");
    }

    #[test]
    fn two_restarts_are_not_a_loop() {
        assert!(run(&[restart(0, "x.service"), restart(30, "x.service")], Some(BOOT_A)).is_empty());
    }

    #[test]
    fn three_restarts_in_five_minutes_are_one_loop_that_keeps_growing() {
        let es: Vec<_> = [0, 100, 200, 290, 350]
            .iter()
            .map(|t| restart(*t, "x.service"))
            .collect();
        let f = run(&es, Some(BOOT_A));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, FindingKind::RestartLoop);
        assert_eq!(f[0].severity, Severity::Medium);
        assert_eq!(f[0].count, 5);
        assert_eq!(f[0].first_us, 0);
        assert_eq!(f[0].last_us, 350 * S);
        assert_eq!(f[0].entries, [0, 1, 2, 3, 4]);
        assert_eq!(f[0].data["unit"], "x.service");
    }

    #[test]
    fn restarts_spread_over_more_than_the_window_are_not_a_loop_and_a_later_burst_is_a_second_episode() {
        let slow: Vec<_> = [0, 200, 400, 600].iter().map(|t| restart(*t, "x.service")).collect();
        assert!(run(&slow, Some(BOOT_A)).is_empty(), "never three within five minutes");

        let mut es: Vec<_> = [0, 10, 20].iter().map(|t| restart(*t, "x.service")).collect();
        es.extend([5000, 5010, 5020].iter().map(|t| restart(*t, "x.service")));
        es.extend([30, 40, 50].iter().map(|t| restart(5000 + *t, "y.service")));
        let f = run(&es, Some(BOOT_A));
        assert_eq!(of(&f, FindingKind::RestartLoop).len(), 3);
    }

    #[test]
    fn a_boot_with_no_shutdown_entry_ended_abruptly_at_its_last_entry() {
        let es = [noise(10, BOOT_A), noise(20, BOOT_A), noise(5000, BOOT_B)];
        let f = run(&es, Some(BOOT_B));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, FindingKind::BootWithoutShutdown);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!((f[0].first_us, f[0].last_us), (20 * S, 20 * S));
        assert_eq!(f[0].boot_id, BOOT_A);
        assert_eq!(f[0].entries, [1]);
        assert_eq!(f[0].evidence, "Last entry: unknown: Started something.");
    }

    #[test]
    fn a_clean_reboot_is_not_flagged_as_power_loss() {
        let es = [noise(10, BOOT_A), shutdown(20, BOOT_A), noise(5000, BOOT_B)];
        assert!(run(&es, Some(BOOT_B)).is_empty());
    }

    #[test]
    fn the_journal_stopping_also_counts_as_an_orderly_end() {
        let stopped = ev(
            20,
            BOOT_A,
            &[("MESSAGE_ID", ids::JOURNAL_STOPPED), ("MESSAGE", "Journal stopped")],
        );
        assert!(run(&[noise(10, BOOT_A), stopped, noise(5000, BOOT_B)], Some(BOOT_B)).is_empty());
    }

    #[test]
    fn the_current_boot_is_never_ended_without_shutdown() {
        let es = [noise(10, BOOT_A), noise(20, BOOT_A)];
        assert!(run(&es, Some(BOOT_A)).is_empty(), "it has not ended yet");
        assert_eq!(
            run(&es, None).len(),
            1,
            "with no current boot given, every boot without a shutdown is reported"
        );
    }

    #[test]
    fn throttle_lines_are_counted_per_boot_at_low_severity() {
        let line = "CPU0: Core temperature above threshold, cpu clock throttled (total events = 1)";
        let f = run(
            &[
                kernel(1, BOOT_A, line),
                kernel(2, BOOT_A, line),
                kernel(3, BOOT_B, line),
                kernel(4, BOOT_B, "CPU0: Core temperature/speed normal"),
            ],
            Some(BOOT_B),
        );
        let t = of(&f, FindingKind::ThermalThrottle);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].count, 2);
        assert_eq!(t[1].count, 1, "the 'normal' line is not a throttle");
        assert_eq!(t[0].severity, Severity::Low);
        assert_eq!(t[0].title, "Thermal throttling (2 events)");
    }

    #[test]
    fn hardware_error_lines_are_high_severity_and_grouped_by_kind() {
        let f = run(
            &[
                kernel(1, BOOT_A, "mce: [Hardware Error]: Machine check events logged"),
                kernel(
                    2,
                    BOOT_A,
                    "pcieport 0000:00:1c.0: AER: Corrected error message received from 0000:00:1c.0",
                ),
                kernel(
                    3,
                    BOOT_A,
                    "pcieport 0000:00:1c.0: AER: Multiple Corrected error received",
                ),
                kernel(4, BOOT_A, "EDAC MC0: 1 CE memory read error on DIMM0"),
                kernel(
                    5,
                    BOOT_A,
                    "blk_update_request: I/O error, dev sda, sector 123 op 0x0:(READ)",
                ),
            ],
            Some(BOOT_A),
        );
        let titles: Vec<_> = f.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Machine check events (1)",
                "PCIe AER errors (2)",
                "Memory (EDAC) errors (1)",
                "Disk I/O errors (1)"
            ]
        );
        assert!(f
            .iter()
            .all(|f| f.severity == Severity::High && f.kind == FindingKind::HardwareError));
    }

    #[test]
    fn the_edac_driver_loading_is_not_a_memory_error() {
        assert!(run(&[kernel(1, BOOT_A, "EDAC MC: Ver: 3.0.0")], Some(BOOT_A)).is_empty());
    }

    #[test]
    fn five_disconnects_of_one_device_within_a_minute_are_a_storm_but_four_or_slow_ones_are_not() {
        let dc = |t, dev: &str| kernel(t, BOOT_A, &format!("{dev}: USB disconnect, device number 7"));
        let storm: Vec<_> = (0..5).map(|i| dc(i * 10, "usb 1-2")).collect();
        let f = run(&storm, Some(BOOT_A));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].data["device"], "usb 1-2");
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].count, 5);

        assert!(
            run(&storm[..4], Some(BOOT_A)).is_empty(),
            "four is a cable being wiggled"
        );
        let slow: Vec<_> = (0..6).map(|i| dc(i * 100, "usb 1-2")).collect();
        assert!(run(&slow, Some(BOOT_A)).is_empty(), "never five within a minute");
        let spread: Vec<_> = (0..6).map(|i| dc(i, &format!("usb 1-{i}"))).collect();
        assert!(run(&spread, Some(BOOT_A)).is_empty(), "different devices do not add up");
    }

    fn failed_login(t_s: u64) -> Entry {
        ev(
            t_s,
            BOOT_A,
            &[
                ("SYSLOG_FACILITY", "10"),
                ("SYSLOG_IDENTIFIER", "sudo"),
                ("MESSAGE", "pam_unix(sudo:auth): authentication failure; logname=alice uid=1000 euid=0 tty=/dev/pts/1 ruser=alice rhost=  user=alice"),
            ],
        )
    }

    #[test]
    fn three_failed_sign_ins_within_ten_minutes_are_flagged_but_two_are_not() {
        assert!(run(&[failed_login(0), failed_login(60)], Some(BOOT_A)).is_empty());
        let f = run(
            &[failed_login(0), failed_login(60), failed_login(120), failed_login(180)],
            Some(BOOT_A),
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, FindingKind::FailedSignIns);
        assert_eq!(f[0].severity, Severity::Medium);
        assert_eq!(f[0].count, 4);
        assert_eq!(f[0].data["service"], "sudo");
    }

    #[test]
    fn failures_spread_over_an_hour_and_successful_logins_are_not_flagged() {
        let slow: Vec<_> = [0, 700, 1400, 2100].iter().map(|t| failed_login(*t)).collect();
        assert!(run(&slow, Some(BOOT_A)).is_empty());
        let ok = |t| {
            ev(
                t,
                BOOT_A,
                &[
                    ("SYSLOG_FACILITY", "10"),
                    ("SYSLOG_IDENTIFIER", "sshd"),
                    ("MESSAGE", "Accepted publickey for alice from 192.0.2.1 port 22 ssh2"),
                ],
            )
        };
        assert!(run(&[ok(0), ok(1), ok(2), ok(3)], Some(BOOT_A)).is_empty());
    }

    #[test]
    fn sshd_failed_passwords_count() {
        let bad = |t| {
            ev(
                t,
                BOOT_A,
                &[
                    ("SYSLOG_IDENTIFIER", "sshd-session"),
                    (
                        "MESSAGE",
                        "Failed password for invalid user bob from 192.0.2.9 port 4000 ssh2",
                    ),
                ],
            )
        };
        let f = run(&[bad(0), bad(5), bad(10)], Some(BOOT_A));
        assert_eq!(f.len(), 1);
    }

    #[test]
    fn a_long_episode_keeps_the_true_count_but_only_a_capped_list_of_entries() {
        let es: Vec<_> = (0..500).map(|i| restart(i, "x.service")).collect();
        let f = run(&es, Some(BOOT_A));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].count, 500);
        assert_eq!(f[0].entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn findings_come_back_in_time_order_with_abrupt_ends_placed_by_their_last_entry() {
        let es = [
            noise(10, BOOT_A),
            noise(20, BOOT_A),
            coredump(500, BOOT_B, "x"),
            coredump(1000, BOOT_B, "y"),
        ];
        let f = run(&es, Some(BOOT_B));
        let kinds: Vec<_> = f.iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            [FindingKind::BootWithoutShutdown, FindingKind::Crash, FindingKind::Crash]
        );
    }

    #[test]
    fn repeats_fold_only_for_the_same_source_and_text() {
        let a = ev(1, BOOT_A, &[("_SYSTEMD_UNIT", "x.service"), ("MESSAGE", "retrying")]);
        let b = ev(2, BOOT_A, &[("_SYSTEMD_UNIT", "x.service"), ("MESSAGE", "retrying")]);
        let other_source = ev(2, BOOT_A, &[("_SYSTEMD_UNIT", "y.service"), ("MESSAGE", "retrying")]);
        let other_text = ev(2, BOOT_A, &[("_SYSTEMD_UNIT", "x.service"), ("MESSAGE", "gave up")]);
        assert!(same_message(&a, &b));
        assert!(!same_message(&a, &other_source));
        assert!(!same_message(&a, &other_text));
    }

    fn cursored(mut e: Entry, n: usize) -> Entry {
        e.fields.insert("__CURSOR".into(), format!("c{n}"));
        e
    }

    fn unit_failed(t_s: u64, boot: &str) -> Entry {
        ev(
            t_s,
            boot,
            &[
                ("MESSAGE_ID", ids::UNIT_FAILURE),
                ("UNIT", "backup.service"),
                ("UNIT_RESULT", "exit-code"),
            ],
        )
    }

    #[test]
    fn a_drained_crash_comes_out_at_once_and_only_once() {
        let mut d = Detector::new();
        d.push(0, &cursored(coredump(10, BOOT_A, "Hyprland"), 0));
        let f = d.drain(10 * S);
        assert_eq!(f.len(), 1, "a follower records a crash within one read");
        assert_eq!(f[0].cursor.as_deref(), Some("c0"));
        assert!(d.drain(20 * S).is_empty(), "a second drain would write it twice");
        assert!(
            d.out.is_empty(),
            "nothing can grow a crash, so a long follow does not keep it"
        );
        assert_eq!(d.open_since(), None);
    }

    #[test]
    fn a_restart_loop_read_across_drains_is_one_episode_that_grows() {
        let mut d = Detector::new();
        for (i, t) in [0, 60].into_iter().enumerate() {
            d.push(i, &cursored(restart(t, "x.service"), i));
        }
        assert!(d.drain(60 * S).is_empty());
        assert_eq!(d.open_since(), Some(0), "two restarts may still become a loop");
        d.push(2, &cursored(restart(120, "x.service"), 2));
        let f = d.drain(120 * S);
        assert_eq!((f.len(), f[0].count, f[0].first_us), (1, 3, 0));
        assert_eq!(
            f[0].cursor.as_deref(),
            Some("c0"),
            "keyed by its first entry, read before the drain"
        );
        d.push(3, &cursored(restart(180, "x.service"), 3));
        let grown = d.drain(180 * S);
        assert_eq!(
            (grown.len(), grown[0].count),
            (1, 4),
            "the same episode, grown, not a second one"
        );
        assert_eq!(
            (grown[0].first_us, grown[0].cursor.clone()),
            (f[0].first_us, f[0].cursor.clone())
        );

        assert!(d.drain(1000 * S).is_empty());
        assert_eq!(d.open_since(), None, "the window has closed");
        d.push(4, &cursored(restart(1000, "x.service"), 4));
        assert!(
            d.drain(1000 * S).is_empty(),
            "a restart after the loop ended starts over"
        );
    }

    #[test]
    fn rereading_from_open_since_rebuilds_the_open_episode_with_the_same_first_entry() {
        let es: Vec<_> = [0, 400, 450, 500, 550]
            .iter()
            .enumerate()
            .map(|(i, t)| cursored(restart(*t, "x.service"), i))
            .collect();
        let mut first = Detector::new();
        for (i, e) in es[..4].iter().enumerate() {
            first.push(i, e);
        }
        let loop_start = first.drain(500 * S);
        let since = first.open_since().unwrap();
        assert_eq!(
            since,
            400 * S,
            "the restart at 0 fell out of the window and need not be read again"
        );

        let mut resumed = Detector::new();
        for (i, e) in es.iter().enumerate().filter(|(_, e)| e.t_us >= since) {
            resumed.push(i, e);
        }
        let again = resumed.drain(550 * S);
        assert_eq!(again.len(), 1);
        assert_eq!(
            (again[0].first_us, &again[0].cursor),
            (loop_start[0].first_us, &loop_start[0].cursor)
        );
        assert_eq!(again[0].count, 4, "grown by the entry read after the resume");
    }

    #[test]
    fn a_units_first_failure_is_drained_at_once_and_repeats_in_its_boot_grow_it() {
        let mut d = Detector::new();
        d.push(0, &cursored(unit_failed(10, BOOT_A), 0));
        let f = d.drain(10 * S);
        assert_eq!(
            (f.len(), f[0].count),
            (1, 1),
            "folding must not hold the first failure back"
        );
        d.push(1, &cursored(unit_failed(5000, BOOT_A), 1));
        let f = d.drain(9000 * S);
        assert_eq!((f.len(), f[0].count, f[0].cursor.as_deref()), (1, 2, Some("c0")));
        assert_eq!(d.open_since(), Some(10 * S), "the fold stays open for the whole boot");

        d.push(2, &cursored(unit_failed(20000, BOOT_B), 2));
        let f = d.drain(20000 * S);
        assert_eq!(
            (f.len(), f[0].count, f[0].boot_id.as_str()),
            (1, 1, BOOT_B),
            "a new boot starts over"
        );
        assert_eq!(d.out.len(), 1, "the old boot's fold is closed");
    }

    #[test]
    fn an_oom_kill_is_held_open_only_for_the_merge_window() {
        let mut d = Detector::new();
        d.push(
            0,
            &kernel(100, BOOT_A, "Out of memory: Killed process 1 (a) total-vm:1kB"),
        );
        assert_eq!(d.drain(100 * S).len(), 1);
        assert_eq!(d.open_since(), Some(100 * S), "the unit's report may still follow");
        assert!(d.drain(110 * S).is_empty());
        assert_eq!(d.open_since(), None);
    }

    #[test]
    fn every_kind_has_a_unique_key_and_package_changes_are_low() {
        let kinds = [
            FindingKind::Crash,
            FindingKind::OomKill,
            FindingKind::FailedUnit,
            FindingKind::RestartLoop,
            FindingKind::BootWithoutShutdown,
            FindingKind::ThermalThrottle,
            FindingKind::HardwareError,
            FindingKind::FailedSignIns,
            FindingKind::PackageChange,
        ];
        let mut keys: Vec<_> = kinds.iter().map(|k| k.key()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), kinds.len());
        assert_eq!(FindingKind::PackageChange.severity(), Severity::Low);
    }
}
