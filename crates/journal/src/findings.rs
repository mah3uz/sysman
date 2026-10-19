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
// systemd-coredump logs a dump seconds after the kernel's line; a large process takes longer.
const CRASH_DEDUP_US: u64 = 30 * SECOND_US;
const SHUTDOWN_STRETCH_US: u64 = 30 * 60 * SECOND_US;
const JOURNAL_STOP_STRETCH_US: u64 = 60 * SECOND_US;
const EVIDENCE_CHARS: usize = 200;
// Kernel warnings and worse; informational lines such as "AER: enabled with IRQ" are not errors.
const HARDWARE_MAX_PRIORITY: u8 = 4;
// GPU drivers report hangs and resets at info or notice.
const GPU_MAX_PRIORITY: u8 = 6;

// Every text pattern the rules match on, kernel unless noted.
const OOM_KILL: &[&str] = &["ut of memory: Kill", "Out of memory (oom_kill_allocating_task): Kill"];
const OOM_SUMMARY: u8 = 1;
const OOM_KILL_LINE: u8 = 2;
const OOM_SYSTEM: u8 = 4;
const OOM_USER: u8 = 8;
const OOM_OOMD: u8 = 16;
const OOM_KILL_SUMMARY: &str = "oom-kill:constraint=";
const THROTTLE: &[&str] = &[
    "temperature is above threshold, cpu clock is throttled",
    "temperature above threshold, cpu clock throttled",
];
const CRITICAL_TEMPERATURE: &str = "critical temperature reached";
const USB_DISCONNECT: &str = ": USB disconnect";
const SEGFAULT: &str = ": segfault at ";
const TRAPS: &str = "traps: ";
const KERNEL_START: &str = "Linux version ";
const RESET_REASON: &str = "x86/amd: Previous system reset reason [";
// journald, not native: "File %s corrupted or uncleanly shut down, renaming and replacing."
const UNCLEAN_JOURNAL: &str = "corrupted or uncleanly shut down";
const GPU: &[(&str, &[&str])] = &[
    ("NVIDIA GPU errors (Xid)", &["NVRM: Xid"]),
    ("AMD GPU hangs", &[" timeout, signaled seq=", "GPU reset begin"]),
    ("Intel GPU hangs", &["GPU HANG", "Timedout job"]),
];
const HARDWARE: &[(&str, &[&str])] = &[
    (
        "Machine check events",
        &["[Hardware Error]", "Machine check", "Machine Check"],
    ),
    ("Memory (EDAC) errors", &["EDAC MC"]),
    (
        "NVMe timeouts",
        &["timeout, aborting", "controller is down; will reset"],
    ),
    ("SATA errors", &["exception Emask", "failed command"]),
];
// The per-error lines of aer.c; its other "AER:" lines are setup and detail.
const AER_EVENT: &[&str] = &["severity=", "error message received", "error received"];
const EDAC_ERROR: &[&str] = &[" CE ", " UE ", "error"];
const UNCORRECTED: &[&str] = &["Uncorrect", "|UE|", " UE ", "Fatal", "|PCC|", "context corrupt"];
const CORRECTED: &[&str] = &["Correctable", "Corrected", "|CE|", " CE "];
// blk_print_req_error prints "%s error, dev %s" with every blk_errors[] name; fs/buffer.c its own line.
const BLOCK_ERROR: &[&str] = &[
    " error, dev ",
    "Buffer I/O error on dev ",
    "Buffer I/O error on device ",
];
// Empty optical drives, VM floppies and loop files fail reads routinely.
const NOISY_DEVICES: &[&str] = &["fd", "sr", "loop"];
const SIGNIN_FAILURE: &[&str] = &["Failed password", "authentication failure"];
const AUTH_FACILITIES: &[&str] = &["4", "10"];
const DISPLAY_MANAGERS: &[&str] = &[
    "gdm",
    "gdm3",
    "sddm",
    "lightdm",
    "greetd",
    "ly",
    "lxdm",
    "xdm",
    "display-manager",
];
const RESTARTABLE_SUFFIXES: &[&str] = &["service", "socket", "mount"];

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
                if n > 1 { format!("{once}, {n} times") } else { once }
            }
            FindingKind::RestartLoop => format!("{} keeps restarting ({n} restarts)", self.field("unit")),
            FindingKind::ThermalThrottle if self.data.contains_key("critical") => {
                format!("Critical temperature reached ({n})")
            }
            FindingKind::ThermalThrottle => format!("Thermal throttling ({n} events)"),
            FindingKind::FailedSignIns => format!("Repeated failed sign-ins ({n})"),
            FindingKind::HardwareError if self.data.contains_key("device") => {
                format!("{} disconnected {n} times in a minute", self.field("device"))
            }
            FindingKind::HardwareError => {
                let corrected = self.data.contains_key("corrected") && !self.data.contains_key("uncorrected");
                self.severity = if corrected { Severity::Medium } else { Severity::High };
                match corrected {
                    true => format!("{}, corrected ({n})", self.field("error")),
                    false => format!("{} ({n})", self.field("error")),
                }
            }
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

/// Whether a system or user manager wrote this entry, judged from the fields journald sets itself. Any local user
/// can log a unit failure with a forged `UNIT`; only PID 1 as root, or the reader's own user manager, is believed.
pub fn trusted_manager_entry(e: &Entry, owner_uid: u32) -> bool {
    match e.get("_PID") {
        Some("1") => system_manager(e),
        _ => e.get("_COMM") == Some("systemd") && e.get("_UID").and_then(|u| u.parse().ok()) == Some(owner_uid),
    }
}

fn system_manager(e: &Entry) -> bool {
    e.get("_PID") == Some("1") && e.get("_UID") == Some("0")
}

/// A unit the app may offer to restart: a valid instantiated or plain `.service`, `.socket` or `.mount` name that
/// cannot be read as an option, and not one whose restart ends the user's session.
pub fn restartable_unit(name: &str) -> bool {
    const VALID: &str = ":-_.\\@";
    let Some((prefix, suffix)) = name.rsplit_once('.') else {
        return false;
    };
    let valid = name.len() < 256
        && !prefix.is_empty()
        && !name.starts_with(['-', '@'])
        && RESTARTABLE_SUFFIXES.contains(&suffix)
        && prefix.chars().all(|c| c.is_ascii_alphanumeric() || VALID.contains(c));
    let base = prefix.split_once('@').map_or(prefix, |(b, _)| b);
    let template = prefix.ends_with('@');
    valid && !template && !matches!(base, "user" | "user-runtime-dir") && !DISPLAY_MANAGERS.contains(&base)
}

/// How a past boot ended, from [`boot_end_verdict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootEndVerdict {
    /// The boot's start is not in the journal (vacuumed, or not readable), so its end is not judged.
    Unknown,
    Clean,
    /// No shutdown, and the next boot's journald or reset reason shows it.
    Confirmed,
    /// No shutdown, and its last sleep event is a suspend with no resume.
    EndedWhileSuspended,
    /// No shutdown was logged, and nothing else says why.
    Unconfirmed,
}

impl BootEndVerdict {
    pub fn key(self) -> &'static str {
        match self {
            BootEndVerdict::Unknown => "unknown",
            BootEndVerdict::Clean => "clean",
            BootEndVerdict::Confirmed => "confirmed",
            BootEndVerdict::EndedWhileSuspended => "ended_while_suspended",
            BootEndVerdict::Unconfirmed => "unconfirmed",
        }
    }

    pub fn ended_without_shutdown(self) -> bool {
        !matches!(self, BootEndVerdict::Unknown | BootEndVerdict::Clean)
    }
}

/// What a boot's own entries and the next boot's say about how it ended. Feed the boot's entries to `note` and the
/// following boot's to `note_next`; only the markers matter, so a reader may feed just those, setting `last_us` to
/// the boot's last entry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BootEndFacts {
    /// The kernel's "Linux version" line was seen.
    pub kernel_start: bool,
    pub last_us: u64,
    /// The latest logind shutdown, or PID 1 stopping multi-user.target.
    pub shutdown_us: Option<u64>,
    pub journal_stopped_us: Option<u64>,
    pub sleep_start_us: Option<u64>,
    pub sleep_stop_us: Option<u64>,
    /// The next boot's journald found a file of this one left open.
    pub next_unclean_journal: bool,
    /// The next boot's AMD reset reason text, after the register value.
    pub next_reset_reason: Option<String>,
}

impl BootEndFacts {
    pub fn note(&mut self, e: &Entry) {
        let t = Some(e.t_us);
        self.last_us = self.last_us.max(e.t_us);
        let root = root_entry(e);
        match e.get("MESSAGE_ID") {
            Some(ids::SHUTDOWN) if root => self.shutdown_us = self.shutdown_us.max(t),
            Some(ids::UNIT_STOPPED) if e.get("UNIT") == Some("multi-user.target") && system_manager(e) => {
                self.shutdown_us = self.shutdown_us.max(t);
            }
            Some(ids::JOURNAL_STOPPED) if root => self.journal_stopped_us = self.journal_stopped_us.max(t),
            Some(ids::SLEEP_START) if root => self.sleep_start_us = self.sleep_start_us.max(t),
            Some(ids::SLEEP_STOP) if root => self.sleep_stop_us = self.sleep_stop_us.max(t),
            _ => {}
        }
        if is_kernel(e) && e.message().starts_with(KERNEL_START) {
            self.kernel_start = true;
        }
    }

    pub fn note_next(&mut self, e: &Entry) {
        let message = e.message();
        if is_kernel(e)
            && let Some(rest) = message.strip_prefix(RESET_REASON)
        {
            self.next_reset_reason = rest.split_once("]: ").map(|(_, r)| r.trim().to_string());
        }
        if e.get("SYSLOG_IDENTIFIER") == Some("systemd-journald") && root_entry(e) && message.contains(UNCLEAN_JOURNAL)
        {
            self.next_unclean_journal = true;
        }
    }

    pub fn ended_suspended(&self) -> bool {
        self.sleep_start_us
            .is_some_and(|s| self.sleep_stop_us.is_none_or(|w| w < s))
    }
}

/// A boot ended cleanly when logind's shutdown or PID 1 stopping multi-user.target falls in its final stretch: after
/// its last resume and within 30 minutes of its last entry. journald stopping counts only within a minute of the end,
/// since journald also stops when it is restarted.
pub fn boot_end_verdict(f: &BootEndFacts) -> BootEndVerdict {
    if !f.kernel_start {
        return BootEndVerdict::Unknown;
    }
    let near_end = |t: u64, stretch: u64| t <= f.last_us && f.last_us - t <= stretch;
    let after_resume = |t: u64| f.sleep_stop_us.is_none_or(|w| t > w);
    let shutdown = f
        .shutdown_us
        .is_some_and(|t| after_resume(t) && near_end(t, SHUTDOWN_STRETCH_US));
    let journal = f
        .journal_stopped_us
        .is_some_and(|t| near_end(t, JOURNAL_STOP_STRETCH_US));
    if shutdown || journal {
        return BootEndVerdict::Clean;
    }
    let reset = f
        .next_reset_reason
        .as_deref()
        .is_some_and(|r| decode_reset_reason(r).1 != ResetKind::Orderly);
    if f.next_unclean_journal || reset {
        BootEndVerdict::Confirmed
    } else if f.ended_suspended() {
        BootEndVerdict::EndedWhileSuspended
    } else {
        BootEndVerdict::Unconfirmed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetKind {
    /// The hardware cut power: a thermal trip, the power button held, a shutdown pin.
    PowerCut,
    /// A reset with no orderly path: the reset pin, a watchdog, a sync flood.
    Abrupt,
    /// What a normal power-off or reboot also leaves, or a reason not known.
    Orderly,
}

/// A short name and kind for the text arch/x86/kernel/cpu/amd.c prints after "Previous system reset reason [..]: ".
pub fn decode_reset_reason(text: &str) -> (&str, ResetKind) {
    const REASONS: &[(&str, &str, ResetKind)] = &[
        ("BP_THERMTRIP_L", "thermal trip", ResetKind::PowerCut),
        ("CPU thermal limit", "CPU thermal limit", ResetKind::PowerCut),
        ("power button", "power button held", ResetKind::PowerCut),
        ("shutdown pin", "shutdown pin", ResetKind::PowerCut),
        ("ASF power off", "remote power-off", ResetKind::PowerCut),
        ("BP_SYS_RST_L", "reset button or board reset", ResetKind::Abrupt),
        ("KB_RST_L", "keyboard controller reset", ResetKind::Abrupt),
        ("watchdog", "watchdog", ResetKind::Abrupt),
        ("sync flood", "sync flood", ResetKind::Abrupt),
        ("CPU shutdown event", "CPU shutdown event", ResetKind::Abrupt),
        ("failed boot timer", "failed boot timer", ResetKind::Abrupt),
        ("parity error", "parity error", ResetKind::Abrupt),
        ("warm reset handshake", "failed warm reset", ResetKind::Abrupt),
        ("ASF reset", "remote reset", ResetKind::Abrupt),
        (
            "ACPI power state transition",
            "ACPI power transition",
            ResetKind::Orderly,
        ),
        ("0xCF9", "software reset", ResetKind::Orderly),
        ("PCI reset", "software reset", ResetKind::Orderly),
    ];
    REASONS
        .iter()
        .find(|(needle, ..)| text.contains(needle))
        .map_or((text, ResetKind::Orderly), |(_, label, kind)| (*label, *kind))
}

fn is_kernel(e: &Entry) -> bool {
    e.get("_TRANSPORT") == Some("kernel")
}

// Only root can write kmsg, and journald sets _UID itself.
fn root_entry(e: &Entry) -> bool {
    is_kernel(e) || e.get("_UID") == Some("0")
}

#[derive(Default)]
struct BootSpan {
    id: String,
    first_us: u64,
    last_us: u64,
    last_index: usize,
    last_line: String,
    end: BootEndFacts,
}

struct PendingCrash {
    t: u64,
    index: usize,
    boot: String,
    pid: String,
    comm: String,
    signal: &'static str,
    evidence: String,
    cursor: Option<String>,
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
    /// Open OOM findings (index, sources already folded in), so a later line of another source joins the nearest one.
    open_ooms: Vec<(usize, u8)>,
    /// Kernel crash lines waiting to see whether a core dump of the same pid follows.
    pending_crashes: Vec<PendingCrash>,
    /// (boot, pid, time) of recent core dumps.
    recent_dumps: VecDeque<(String, String, u64)>,
    owner_uid: u32,
}

impl Detector {
    /// Believes the user manager of the user running it.
    pub fn new() -> Detector {
        // SAFETY: geteuid has no preconditions and cannot fail.
        Detector::for_owner(unsafe { libc::geteuid() })
    }

    /// Believes the system manager and the user manager running as `owner_uid`.
    pub fn for_owner(owner_uid: u32) -> Detector {
        Detector {
            owner_uid,
            ..Detector::default()
        }
    }

    pub fn push(&mut self, index: usize, e: &Entry) {
        let boot = e.get("_BOOT_ID").unwrap_or_default();
        self.track_boot(index, e, boot);
        let t = e.t_us;
        self.flush_crashes(t);
        let before = self.out.len();
        let trusted = trusted_manager_entry(e, self.owner_uid);
        match e.get("MESSAGE_ID") {
            Some(ids::COREDUMP) => self.crash(index, e, boot),
            Some(ids::UNIT_FAILURE) if trusted => self.failed_unit(index, e, boot),
            Some(ids::RESTART_SCHEDULED) if trusted => self.restart(index, e, boot),
            Some(ids::UNIT_OOM) if trusted => self.oom(index, e, boot),
            Some(ids::OOMD_KILL) => self.oom(index, e, boot),
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
        self.flush_crashes(now_us);
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
        self.open_ooms
            .retain(|&(i, _)| !ended(self.out[i].last_us, OOM_MERGE_US));

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
        for o in &mut self.open_ooms {
            o.0 = moved[o.0];
        }
        self.reported = self.out.iter().map(|f| f.count).collect();
        fresh
    }

    /// The earliest time a still-open rule depends on, as of the last `drain`. A new detector fed from it rebuilds the
    /// open findings with the same first entries, so a reader that stops and resumes there neither splits nor
    /// repeats an episode.
    pub fn open_since(&self) -> Option<u64> {
        let quiet = self.bursts.values().filter_map(|b| b.events.front().map(|h| h.t));
        let pending = self.pending_crashes.iter().map(|p| p.t);
        self.held()
            .map(|i| self.out[i].first_us)
            .chain(quiet)
            .chain(pending)
            .min()
    }

    fn held(&self) -> impl Iterator<Item = usize> + '_ {
        let bursts = self.bursts.values().filter_map(|b| b.finding);
        self.failed
            .values()
            .chain(self.grouped.values())
            .copied()
            .chain(bursts)
            .chain(self.open_ooms.iter().map(|o| o.0))
    }

    /// `current_boot` is never reported as ended without shutdown, it has not ended yet.
    pub fn finish(mut self, current_boot: Option<&str>) -> Vec<Finding> {
        self.flush_crashes(u64::MAX);
        for span in &self.boots {
            if span.id.is_empty() || Some(span.id.as_str()) == current_boot {
                continue;
            }
            let verdict = boot_end_verdict(&span.end);
            if verdict.ended_without_shutdown() {
                self.out.push(boot_end_finding(span, verdict));
            }
        }
        self.out.sort_by_key(|f| f.first_us);
        self.out
    }

    /// Makes findings of the kernel crash lines that no core dump followed within the window, as of `now_us`.
    fn flush_crashes(&mut self, now_us: u64) {
        let ripe = |p: &PendingCrash| now_us.saturating_sub(p.t) > CRASH_DEDUP_US;
        while self
            .recent_dumps
            .front()
            .is_some_and(|d| now_us.saturating_sub(d.2) > CRASH_DEDUP_US)
        {
            self.recent_dumps.pop_front();
        }
        if !self.pending_crashes.iter().any(ripe) {
            return;
        }
        let (done, wait): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending_crashes).into_iter().partition(ripe);
        self.pending_crashes = wait;
        for p in done {
            let mut f = self.crash_finding(p.t, p.index, &p.boot, &p.comm, p.signal, p.evidence);
            f.data.insert("pid".to_string(), p.pid);
            f.data.insert("signal".to_string(), p.signal.to_string());
            f.data.insert("source".to_string(), "kernel".to_string());
            f.cursor = p.cursor;
            self.out.push(f);
        }
    }

    fn kernel_crash(&mut self, index: usize, e: &Entry, boot: &str, (comm, pid, signal): (&str, &str, &'static str)) {
        let dumped = self.recent_dumps.iter().any(|d| d.0 == boot && d.1 == pid);
        if dumped {
            return;
        }
        self.pending_crashes.push(PendingCrash {
            t: e.t_us,
            index,
            boot: boot.to_string(),
            pid: pid.to_string(),
            comm: comm.to_string(),
            signal,
            evidence: evidence(e.message()),
            cursor: e.get("__CURSOR").map(str::to_string),
        });
    }

    fn crash_finding(&mut self, t: u64, index: usize, boot: &str, comm: &str, signal: &str, shown: String) -> Finding {
        let n = self.crashes.entry((boot.to_string(), comm.to_string())).or_default();
        *n += 1;
        let title = match *n {
            1 => format!("{comm} crashed ({signal})"),
            2 => format!("Second {comm} crash this boot ({signal})"),
            3 => format!("Third {comm} crash this boot ({signal})"),
            n => format!("{} {comm} crash this boot ({signal})", ordinal(n)),
        };
        Finding::new(FindingKind::Crash, t, index, boot, title, shown).with("comm", comm)
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
        span.end.note(e);
        if let Some(previous) = self.current.checked_sub(1) {
            self.boots[previous].end.note_next(e);
        }
    }

    fn crash(&mut self, index: usize, e: &Entry, boot: &str) {
        let comm = e.get("COREDUMP_COMM").unwrap_or("A process");
        let signal = e.get("COREDUMP_SIGNAL_NAME").unwrap_or("unknown signal");
        if let Some(pid) = e.get("COREDUMP_PID") {
            self.pending_crashes.retain(|p| p.boot != boot || p.pid != pid);
            self.recent_dumps.push_back((boot.to_string(), pid.to_string(), e.t_us));
        }
        let mut shown = evidence(e.message());
        if shown.is_empty() {
            // The reader may drop a long multi-line MESSAGE.
            shown = format!(
                "{comm} (pid {}) received {signal}",
                e.get("COREDUMP_PID").unwrap_or("?")
            );
        }
        let mut f = self.crash_finding(e.t_us, index, boot, comm, signal, shown);
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
        if OOM_KILL.iter().any(|p| message.contains(p)) || message.contains(OOM_KILL_SUMMARY) {
            return self.oom(index, e, boot);
        }
        if let Some(crash) = kernel_crash_line(message) {
            return self.kernel_crash(index, e, boot, crash);
        }
        let critical = message.contains(CRITICAL_TEMPERATURE);
        if critical || THROTTLE.iter().any(|p| message.contains(p)) {
            let key = if critical { "critical" } else { "" };
            return self.group(("thermal", boot.to_string(), key.to_string()), index, e, || {
                let f = Finding::new(
                    FindingKind::ThermalThrottle,
                    e.t_us,
                    index,
                    boot,
                    String::new(),
                    evidence(message),
                );
                if critical { f.with("critical", "true") } else { f }
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
        if let Some((what, class)) = hardware_error(e) {
            let key = ("hardware", boot.to_string(), what.to_string());
            self.group(key.clone(), index, e, || {
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
            if let Some(class) = class {
                let f = &mut self.out[self.grouped[&key]];
                f.data.insert(class.to_string(), "true".to_string());
                f.retitle();
            }
        }
    }

    // One kill is logged by the kernel and reported by the system manager, the user manager and systemd-oomd, within
    // seconds of each other and in either order. A line joins the nearest open kill that lacks its source and is not
    // another pid's; two kills in quick succession each have their own kernel lines and stay two.
    fn oom(&mut self, index: usize, e: &Entry, boot: &str) {
        let message = e.message();
        let (pid, comm) = oom_victim(message);
        let source = match e.get("MESSAGE_ID") {
            Some(ids::UNIT_OOM) if system_manager(e) => OOM_SYSTEM,
            Some(ids::UNIT_OOM) => OOM_USER,
            Some(ids::OOMD_KILL) => OOM_OOMD,
            _ if message.contains(OOM_KILL_SUMMARY) => OOM_SUMMARY,
            _ => OOM_KILL_LINE,
        };
        let mut extra = Vec::new();
        if let Some(p) = pid.clone() {
            extra.push(("pid", p));
        }
        if let Some(c) = comm {
            extra.push(("comm", c));
        }
        if let Some(u) = e.get("UNIT").or_else(|| e.get("USER_UNIT")) {
            extra.push(("unit", u.to_string()));
        }
        let t = e.t_us;
        let gap = |f: &Finding| {
            if t < f.first_us {
                f.first_us - t
            } else {
                t.saturating_sub(f.last_us)
            }
        };
        let merge = self
            .open_ooms
            .iter()
            .enumerate()
            .filter(|&(_, &(i, seen))| {
                let f = &self.out[i];
                let other_pid = pid
                    .as_deref()
                    .is_some_and(|p| !f.field("pid").is_empty() && f.field("pid") != p);
                seen & source == 0 && f.boot_id == boot && gap(f) <= OOM_MERGE_US && !other_pid
            })
            .min_by_key(|&(_, &(i, _))| (gap(&self.out[i]), std::cmp::Reverse(self.out[i].last_us)))
            .map(|(k, _)| k);
        let i = match merge {
            Some(k) => {
                let i = self.open_ooms[k].0;
                let last = self.out[i].last_us;
                self.out[i].add(t, index);
                self.out[i].last_us = last.max(t);
                self.open_ooms[k].1 |= source;
                i
            }
            None => {
                self.out.push(Finding::new(
                    FindingKind::OomKill,
                    t,
                    index,
                    boot,
                    String::new(),
                    evidence(message),
                ));
                self.open_ooms.push((self.out.len() - 1, source));
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

/// The hardware rule a kernel line belongs to, and whether it says the error was corrected.
fn hardware_error(e: &Entry) -> Option<(&'static str, Option<&'static str>)> {
    let message = e.message();
    let has = |needles: &[&str]| needles.iter().any(|n| message.contains(n));
    let priority = e.priority();
    if priority <= GPU_MAX_PRIORITY
        && let Some((what, _)) = GPU.iter().find(|(_, needles)| has(needles))
    {
        return Some((what, None));
    }
    if priority > HARDWARE_MAX_PRIORITY {
        return None;
    }
    let class = if has(UNCORRECTED) {
        Some("uncorrected")
    } else if has(CORRECTED) {
        Some("corrected")
    } else {
        None
    };
    if message.contains("AER: ") {
        return has(AER_EVENT).then_some(("PCIe AER errors", class));
    }
    if let Some(device) = block_error_device(message) {
        let noise = NOISY_DEVICES.iter().any(|d| device.starts_with(d));
        return (!noise).then_some(("Disk I/O errors", None));
    }
    HARDWARE.iter().find_map(|(what, needles)| {
        let edac_noise = *what == "Memory (EDAC) errors" && !has(EDAC_ERROR);
        (has(needles) && !edac_noise).then_some((*what, class))
    })
}

fn block_error_device(message: &str) -> Option<&str> {
    let (_, rest) = BLOCK_ERROR.iter().find_map(|n| message.split_once(n))?;
    rest.split([',', ' ']).next()
}

/// (comm, pid, signal) of arch/x86/mm/fault.c's "comm[pid]: segfault at" or arch/x86/kernel/traps.c's
/// "traps: comm[pid] trap ..." line.
fn kernel_crash_line(message: &str) -> Option<(&str, &str, &'static str)> {
    if let Some(at) = message.find(SEGFAULT) {
        let (comm, pid) = comm_pid(&message[..at])?;
        return Some((comm, pid, "SIGSEGV"));
    }
    let rest = message.strip_prefix(TRAPS)?;
    let close = rest.find("] ")?;
    let (comm, pid) = comm_pid(&rest[..=close])?;
    let what = &rest[close + 2..];
    let signal = match what {
        w if w.starts_with("trap invalid opcode") => "SIGILL",
        w if w.starts_with("trap divide error") => "SIGFPE",
        w if ["segment not present", "stack segment", "alignment check"]
            .iter()
            .any(|t| w.contains(t)) =>
        {
            "SIGBUS"
        }
        _ => "SIGSEGV",
    };
    Some((comm, pid, signal))
}

fn comm_pid(s: &str) -> Option<(&str, &str)> {
    let (comm, pid) = s.strip_suffix(']')?.rsplit_once('[')?;
    let numeric = !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit());
    (numeric && !comm.is_empty()).then_some((comm, pid))
}

fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn boot_end_finding(span: &BootSpan, verdict: BootEndVerdict) -> Finding {
    let reason = span.end.next_reset_reason.as_deref();
    let decoded = reason.map(decode_reset_reason);
    let title = match decoded {
        Some((label, ResetKind::PowerCut)) => format!("Power lost: {label}"),
        _ if span.end.ended_suspended() => "Ended while suspended".to_string(),
        _ => "Ended without shutdown".to_string(),
    };
    let evidence = match reason {
        Some(r) => format!("Next boot's reset reason: {r}. Last entry: {}", span.last_line),
        None => format!("Last entry: {}", span.last_line),
    };
    let mut f = Finding::new(
        FindingKind::BootWithoutShutdown,
        span.last_us,
        span.last_index,
        &span.id,
        title,
        evidence,
    )
    .with("boot_started_us", span.first_us.to_string())
    .with("certainty", verdict.key());
    if let Some((label, _)) = decoded {
        f = f.with("reset_reason", label);
    }
    if span.end.ended_suspended() {
        f = f.with("suspended", "true");
    }
    f
}

fn oom_victim(message: &str) -> (Option<String>, Option<String>) {
    if let Some(at) = message.find("task=") {
        let rest = &message[at + 5..];
        let comm = rest.split(',').next().map(str::to_string);
        let pid = rest.split("pid=").nth(1).map(digits);
        return (pid.filter(|p| !p.is_empty()), comm.filter(|c| !c.is_empty()));
    }
    if let Some(at) = message.find("Killed process ") {
        let rest = &message[at + "Killed process ".len()..];
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

    fn kernel_at(t_s: u64, boot: &str, priority: &str, message: &str) -> Entry {
        let mut e = kernel(t_s, boot, message);
        e.fields.insert("PRIORITY".into(), priority.into());
        e
    }

    const PID1: &[(&str, &str)] = &[("_PID", "1"), ("_UID", "0"), ("_COMM", "systemd")];

    /// An entry as PID 1 writes it, with the fields journald sets.
    fn manager(t_s: u64, boot: &str, fields: &[(&str, &str)]) -> Entry {
        let mut e = ev(t_s, boot, fields);
        for (k, v) in PID1 {
            e.fields.insert(k.to_string(), v.to_string());
        }
        e
    }

    fn boot_start(t_s: u64, boot: &str) -> Entry {
        kernel(
            t_s,
            boot,
            "Linux version 6.12.0 (builder@example) (gcc 14.2.0) #1 SMP PREEMPT_DYNAMIC",
        )
    }

    fn restart(t_s: u64, unit: &str) -> Entry {
        manager(
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
            &[
                ("MESSAGE_ID", ids::SHUTDOWN),
                ("MESSAGE", "System is powering down."),
                ("_COMM", "systemd-logind"),
                ("_UID", "0"),
                ("_PID", "812"),
            ],
        )
    }

    fn multi_user_stopped(t_s: u64, boot: &str) -> Entry {
        manager(
            t_s,
            boot,
            &[
                ("MESSAGE_ID", ids::UNIT_STOPPED),
                ("UNIT", "multi-user.target"),
                ("MESSAGE", "Stopped target multi-user.target - Multi-User System."),
            ],
        )
    }

    fn journal_stopped(t_s: u64, boot: &str) -> Entry {
        ev(
            t_s,
            boot,
            &[
                ("MESSAGE_ID", ids::JOURNAL_STOPPED),
                ("MESSAGE", "Journal stopped"),
                ("_COMM", "systemd-journal"),
                ("_UID", "0"),
                ("_TRANSPORT", "driver"),
            ],
        )
    }

    fn sleep(t_s: u64, boot: &str, id: &str) -> Entry {
        ev(
            t_s,
            boot,
            &[("MESSAGE_ID", id), ("_COMM", "systemd-sleep"), ("_UID", "0")],
        )
    }

    fn abrupt(f: &[Finding]) -> Vec<&Finding> {
        of(f, FindingKind::BootWithoutShutdown)
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
                kernel(
                    100,
                    BOOT_A,
                    "oom-kill:constraint=CONSTRAINT_NONE,nodemask=(null),cpuset=/,mems_allowed=0,global_oom,task_memcg=/user.slice,task=firefox,pid=777,uid=1000",
                ),
                kernel(
                    100,
                    BOOT_A,
                    "Out of memory: Killed process 777 (firefox) total-vm:9000kB, anon-rss:8000kB",
                ),
                manager(
                    101,
                    BOOT_A,
                    &[
                        ("MESSAGE_ID", ids::UNIT_OOM),
                        ("UNIT", "app.service"),
                        (
                            "MESSAGE",
                            "app.service: A process of this unit has been killed by the OOM killer.",
                        ),
                    ],
                ),
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

    fn oom_lines(t_s: u64, pid: &str, comm: &str) -> [Entry; 2] {
        [
            kernel(
                t_s,
                BOOT_A,
                &format!(
                    "oom-kill:constraint=CONSTRAINT_NONE,nodemask=(null),cpuset=/,mems_allowed=0,global_oom,task_memcg=/user.slice,task={comm},pid={pid},uid=1000"
                ),
            ),
            kernel(
                t_s,
                BOOT_A,
                &format!("Out of memory: Killed process {pid} ({comm}) total-vm:9000kB"),
            ),
        ]
    }

    fn user_manager_oom(t_s: u64, unit: &str) -> Entry {
        let mut e = ev(
            t_s,
            BOOT_A,
            &[
                ("MESSAGE_ID", ids::UNIT_OOM),
                ("USER_UNIT", unit),
                ("MESSAGE", "A process of this unit has been killed by the OOM killer."),
            ],
        );
        e.fields.insert("_PID".into(), "900".into());
        e.fields.insert("_COMM".into(), "systemd".into());
        e.fields.insert("_UID".into(), unsafe { libc::geteuid() }.to_string());
        e
    }

    fn system_manager_oom(t_s: u64, unit: &str) -> Entry {
        manager(
            t_s,
            BOOT_A,
            &[
                ("MESSAGE_ID", ids::UNIT_OOM),
                ("UNIT", unit),
                ("MESSAGE", "A process of this unit has been killed by the OOM killer."),
            ],
        )
    }

    #[test]
    fn one_kill_logged_by_the_kernel_and_the_user_manager_is_one_finding_in_either_order_and_through_drain() {
        let [s, k] = oom_lines(100, "777", "firefox");
        let m = user_manager_oom(103, "app-firefox.scope");
        let f = run(&[s.clone(), k.clone(), m.clone()], Some(BOOT_A));
        let oom = of(&f, FindingKind::OomKill);
        assert_eq!(oom.len(), 1, "{f:#?}");
        assert_eq!(
            (oom[0].data["comm"].as_str(), oom[0].data["unit"].as_str()),
            ("firefox", "app-firefox.scope")
        );
        assert_eq!(oom[0].count, 3);

        let early = user_manager_oom(99, "app-firefox.scope");
        assert_eq!(
            of(
                &run(&[early.clone(), s.clone(), k.clone()], Some(BOOT_A)),
                FindingKind::OomKill
            )
            .len(),
            1
        );

        let mut d = Detector::new();
        for (i, e) in [&s, &k, &m].into_iter().enumerate() {
            d.push(i, e);
        }
        let mut seen = d.drain(104 * S);
        seen.extend(d.drain(200 * S));
        let last = seen.iter().rfind(|f| f.kind == FindingKind::OomKill).unwrap();
        assert_eq!(
            seen.iter()
                .filter(|f| f.kind == FindingKind::OomKill && f.first_us == 100 * S)
                .count(),
            1
        );
        assert_eq!(last.count, 3);
        assert!(d.finish(Some(BOOT_A)).is_empty(), "drained and closed");
    }

    #[test]
    fn the_system_manager_and_oomd_reports_of_the_same_kill_fold_in_too() {
        let [s, k] = oom_lines(100, "5", "big");
        let sys = system_manager_oom(101, "user@1000.service");
        let usr = user_manager_oom(102, "app.scope");
        let oomd = ev(
            102,
            BOOT_A,
            &[
                ("MESSAGE_ID", ids::OOMD_KILL),
                ("UNIT", "app.scope"),
                ("MESSAGE", "Killed /app.scope due to memory pressure"),
            ],
        );
        let f = run(&[s, k, sys, usr, oomd], Some(BOOT_A));
        let oom = of(&f, FindingKind::OomKill);
        assert_eq!((oom.len(), oom[0].count), (1, 5), "{f:#?}");
    }

    #[test]
    fn two_kills_in_quick_succession_stay_two_each_with_its_own_unit() {
        let [s1, k1] = oom_lines(100, "1", "a");
        let [s2, k2] = oom_lines(102, "2", "b");
        let f = run(
            &[
                s1,
                k1,
                user_manager_oom(101, "a.scope"),
                user_manager_oom(102, "b.scope"),
                s2,
                k2,
            ],
            Some(BOOT_A),
        );
        let oom = of(&f, FindingKind::OomKill);
        assert_eq!(oom.len(), 2, "{f:#?}");
        assert_eq!(
            (oom[0].data["comm"].as_str(), oom[0].data["unit"].as_str()),
            ("a", "a.scope")
        );
        assert_eq!(
            (oom[1].data["comm"].as_str(), oom[1].data["unit"].as_str()),
            ("b", "b.scope")
        );
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
            manager(
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
    fn a_power_cut_boot_with_no_shutdown_is_an_unconfirmed_abrupt_end_at_its_last_entry() {
        let es = [
            boot_start(1, BOOT_A),
            noise(10, BOOT_A),
            noise(20, BOOT_A),
            boot_start(5000, BOOT_B),
        ];
        let f = run(&es, Some(BOOT_B));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, FindingKind::BootWithoutShutdown);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!((f[0].first_us, f[0].last_us), (20 * S, 20 * S));
        assert_eq!(f[0].boot_id, BOOT_A);
        assert_eq!(f[0].entries, [2]);
        assert_eq!(f[0].evidence, "Last entry: unknown: Started something.");
        assert_eq!(f[0].title, "Ended without shutdown", "nothing proves the power went");
        assert_eq!(f[0].data["certainty"], "unconfirmed");
        assert_eq!(f[0].data["boot_started_us"], (S).to_string());
    }

    #[test]
    fn a_clean_reboot_is_not_flagged_as_power_loss() {
        let es = [
            boot_start(1, BOOT_A),
            noise(10, BOOT_A),
            shutdown(20, BOOT_A),
            noise(30, BOOT_A),
            boot_start(5000, BOOT_B),
        ];
        assert!(run(&es, Some(BOOT_B)).is_empty());
    }

    #[test]
    fn ctrl_alt_del_shutdown_is_clean_via_multi_user_target_stopped() {
        // PID 1 starts ctrl-alt-del.target itself, so logind logs no shutdown.
        let es = [
            boot_start(1, BOOT_A),
            noise(10, BOOT_A),
            multi_user_stopped(20, BOOT_A),
            noise(25, BOOT_A),
            boot_start(5000, BOOT_B),
        ];
        assert!(run(&es, Some(BOOT_B)).is_empty());
    }

    #[test]
    fn a_forged_shutdown_marker_does_not_hide_a_power_cut() {
        let mut fake_stop = multi_user_stopped(20, BOOT_A);
        fake_stop.fields.insert("_PID".into(), "4242".into());
        fake_stop.fields.insert("_UID".into(), "1000".into());
        let mut fake_logind = shutdown(21, BOOT_A);
        fake_logind.fields.insert("_UID".into(), "1000".into());
        let es = [boot_start(1, BOOT_A), fake_stop, fake_logind, boot_start(5000, BOOT_B)];
        assert_eq!(abrupt(&run(&es, Some(BOOT_B))).len(), 1);
    }

    #[test]
    fn a_journald_restart_mid_boot_doesnt_make_a_power_cut_boot_clean() {
        let es = [
            boot_start(1, BOOT_A),
            journal_stopped(100, BOOT_A),
            noise(100 + 3600, BOOT_A),
            boot_start(9000, BOOT_B),
        ];
        assert_eq!(
            abrupt(&run(&es, Some(BOOT_B))).len(),
            1,
            "journald stopped an hour before the end"
        );
    }

    #[test]
    fn the_journal_stopping_in_the_last_minute_counts_as_an_orderly_end() {
        let es = [
            boot_start(1, BOOT_A),
            noise(10, BOOT_A),
            journal_stopped(20, BOOT_A),
            noise(50, BOOT_A),
            boot_start(5000, BOOT_B),
        ];
        assert!(run(&es, Some(BOOT_B)).is_empty());
    }

    #[test]
    fn a_shutdown_marker_followed_by_a_resume_or_long_activity_is_not_the_end() {
        // soft-reboot logs a shutdown but keeps the boot id; the boot went on and was later cut
        let resumed = [
            boot_start(1, BOOT_A),
            shutdown(10, BOOT_A),
            sleep(20, BOOT_A, ids::SLEEP_START),
            sleep(30, BOOT_A, ids::SLEEP_STOP),
            noise(40, BOOT_A),
            boot_start(5000, BOOT_B),
        ];
        assert_eq!(abrupt(&run(&resumed, Some(BOOT_B))).len(), 1);
        let long = [
            boot_start(1, BOOT_A),
            shutdown(10, BOOT_A),
            noise(10 + 31 * 60, BOOT_A),
            boot_start(9000, BOOT_B),
        ];
        assert_eq!(abrupt(&run(&long, Some(BOOT_B))).len(), 1);
    }

    #[test]
    fn a_boot_whose_start_was_vacuumed_away_is_not_judged() {
        let es = [noise(10, BOOT_A), noise(20, BOOT_A), boot_start(5000, BOOT_B)];
        assert!(
            run(&es, Some(BOOT_B)).is_empty(),
            "its shutdown may have gone with its start"
        );
        let mut facts = BootEndFacts::default();
        facts.note(&noise(10, BOOT_A));
        assert_eq!(boot_end_verdict(&facts), BootEndVerdict::Unknown);
    }

    #[test]
    fn the_next_boots_unclean_journal_line_confirms_the_abrupt_end() {
        let unclean = kernel_at(
            5001,
            BOOT_B,
            "4",
            "File /var/log/journal/0123/system.journal corrupted or uncleanly shut down, renaming and replacing.",
        );
        let mut unclean = unclean;
        unclean
            .fields
            .insert("SYSLOG_IDENTIFIER".into(), "systemd-journald".into());
        let es = [
            boot_start(1, BOOT_A),
            noise(20, BOOT_A),
            boot_start(5000, BOOT_B),
            unclean,
        ];
        let f = run(&es, Some(BOOT_B));
        assert_eq!(f[0].data["certainty"], "confirmed");
        assert_eq!(f[0].title, "Ended without shutdown");
    }

    #[test]
    fn an_amd_thermal_trip_reset_reason_is_named_and_is_the_only_power_lost_title() {
        // arch/x86/kernel/cpu/amd.c: "x86/amd: Previous system reset reason [0x%08x]: %s\n"
        let reason = |text: &str| {
            kernel(
                5001,
                BOOT_B,
                &format!("x86/amd: Previous system reset reason [0x00000001]: {text}"),
            )
        };
        let run_with = |r| {
            run(
                &[boot_start(1, BOOT_A), noise(20, BOOT_A), boot_start(5000, BOOT_B), r],
                Some(BOOT_B),
            )
        };

        let f = run_with(reason("thermal pin BP_THERMTRIP_L was tripped"));
        assert_eq!(f[0].title, "Power lost: thermal trip");
        assert_eq!(f[0].data["certainty"], "confirmed");
        assert_eq!(f[0].data["reset_reason"], "thermal trip");
        assert!(f[0].evidence.contains("BP_THERMTRIP_L"), "{}", f[0].evidence);

        let f = run_with(reason("system reset pin BP_SYS_RST_L was tripped"));
        assert_eq!(
            (f[0].title.as_str(), f[0].data["certainty"].as_str()),
            ("Ended without shutdown", "confirmed")
        );
        assert_eq!(f[0].data["reset_reason"], "reset button or board reset");

        let f = run_with(reason("software wrote 0x6 to reset control register 0xCF9"));
        assert_eq!(
            f[0].data["certainty"], "unconfirmed",
            "a software reboot leaves the same reason as a normal one"
        );
        assert_eq!(f[0].data["reset_reason"], "software reset");
        assert!(f[0].evidence.starts_with("Next boot's reset reason: software wrote"));
    }

    #[test]
    fn a_boot_that_ended_suspended_says_so() {
        let es = [
            boot_start(1, BOOT_A),
            sleep(10, BOOT_A, ids::SLEEP_START),
            sleep(20, BOOT_A, ids::SLEEP_STOP),
            sleep(30, BOOT_A, ids::SLEEP_START),
            boot_start(90000, BOOT_B),
        ];
        let f = run(&es, Some(BOOT_B));
        assert_eq!(f[0].title, "Ended while suspended");
        assert_eq!(f[0].data["certainty"], "ended_while_suspended");
        assert_eq!(f[0].data["suspended"], "true");
    }

    #[test]
    fn the_current_boot_is_never_ended_without_shutdown() {
        let es = [boot_start(1, BOOT_A), noise(10, BOOT_A), noise(20, BOOT_A)];
        assert!(run(&es, Some(BOOT_A)).is_empty(), "it has not ended yet");
        assert_eq!(
            run(&es, None).len(),
            1,
            "with no current boot given, every boot without a shutdown is reported"
        );
    }

    #[test]
    fn an_intel_throttle_line_from_a_current_kernel_is_found_per_boot_at_low_severity() {
        // drivers/thermal/intel/therm_throt.c
        let line = "CPU0: Core temperature is above threshold, cpu clock is throttled (total events = 1)";
        let old = "CPU0: Package temperature above threshold, cpu clock throttled (total events = 9)";
        let f = run(
            &[
                kernel(1, BOOT_A, line),
                kernel(2, BOOT_A, old),
                kernel(3, BOOT_B, line),
                kernel(4, BOOT_B, "CPU0: Core temperature/speed normal (total events = 1)"),
            ],
            Some(BOOT_B),
        );
        let t = of(&f, FindingKind::ThermalThrottle);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].count, 2, "the pre-5.x text still counts");
        assert_eq!(t[1].count, 1, "the 'normal' line is not a throttle");
        assert_eq!(t[0].severity, Severity::Low);
        assert_eq!(t[0].title, "Thermal throttling (2 events)");
    }

    #[test]
    fn a_thermal_zone_reaching_its_critical_temperature_is_found_on_any_vendor() {
        // drivers/thermal/thermal_core.c: "%s: critical temperature reached, shutting down"
        let f = run(
            &[kernel_at(
                1,
                BOOT_A,
                "0",
                "thermal thermal_zone0: acpitz: critical temperature reached, shutting down",
            )],
            Some(BOOT_A),
        );
        assert_eq!(f.len(), 1);
        assert_eq!(
            (f[0].kind, f[0].severity),
            (FindingKind::ThermalThrottle, Severity::Low)
        );
        assert_eq!(f[0].title, "Critical temperature reached (1)");
    }

    #[test]
    fn hardware_error_lines_are_grouped_by_kind_and_corrected_ones_are_medium() {
        let f = run(
            &[
                kernel_at(
                    1,
                    BOOT_A,
                    "0",
                    "mce: [Hardware Error]: CPU 2: Machine Check: 0 Bank 5: be00000000800400",
                ),
                kernel_at(
                    2,
                    BOOT_A,
                    "4",
                    "pcieport 0000:00:1c.0: AER: PCIe Bus Error: severity=Correctable, type=Physical Layer, (Receiver ID)",
                ),
                kernel_at(
                    3,
                    BOOT_A,
                    "4",
                    "pcieport 0000:00:1c.0: AER:   device [8086:a33c] error status/mask=00000001/00002000",
                ),
                kernel_at(
                    4,
                    BOOT_A,
                    "4",
                    "EDAC MC0: 1 CE memory read error on DIMM0 (channel:0 slot:0 page:0x0)",
                ),
                kernel_at(
                    5,
                    BOOT_A,
                    "3",
                    "I/O error, dev sda, sector 123 op 0x0:(READ) flags 0x0 phys_seg 1 prio class 2",
                ),
            ],
            Some(BOOT_A),
        );
        let got: Vec<_> = f.iter().map(|f| (f.title.as_str(), f.severity)).collect();
        assert_eq!(
            got,
            [
                ("Machine check events (1)", Severity::High),
                ("PCIe AER errors, corrected (1)", Severity::Medium),
                ("Memory (EDAC) errors, corrected (1)", Severity::Medium),
                ("Disk I/O errors (1)", Severity::High),
            ],
            "the AER detail line is not a second error"
        );
        assert!(f.iter().all(|f| f.kind == FindingKind::HardwareError));
    }

    #[test]
    fn aer_enabled_with_irq_at_boot_is_not_a_hardware_error() {
        // drivers/pci/pcie/aer.c: pci_info(port, "enabled with IRQ %d\n") under pr_fmt "AER: "
        for priority in ["6", "4"] {
            let line = kernel_at(1, BOOT_A, priority, "pcieport 0000:00:01.1: AER: enabled with IRQ 28");
            assert!(run(&[line], Some(BOOT_A)).is_empty(), "priority {priority}");
        }
    }

    #[test]
    fn informational_hardware_lines_are_not_errors() {
        let f = run(
            &[
                kernel_at(1, BOOT_A, "6", "mce: [Hardware Error]: Machine check events logged"),
                kernel_at(
                    2,
                    BOOT_A,
                    "6",
                    "EDAC MC0: Giving out device to module amd64_edac controller F19h: DEV 0000:00:18.3 (INTERRUPT)",
                ),
            ],
            Some(BOOT_A),
        );
        assert!(f.is_empty(), "{f:#?}");
    }

    #[test]
    fn corrected_and_uncorrected_errors_take_their_own_severity() {
        // drivers/edac/mce_amd.c prints every line with pr_emerg; drivers/pci/pcie/aer.c logs uncorrected at KERN_ERR
        let amd_ce = run(
            &[
                kernel_at(
                    1,
                    BOOT_A,
                    "0",
                    "mce: [Hardware Error]: Corrected error, no action required.",
                ),
                kernel_at(
                    1,
                    BOOT_A,
                    "0",
                    "mce: [Hardware Error]: CPU:0 (19:21:0) MC9_STATUS[-|CE|MiscV|AddrV|-|-|-|-|CECC|-|-|-]: 0x9c2040000000011b",
                ),
                kernel_at(1, BOOT_A, "0", "mce: [Hardware Error]: Error Addr: 0x00000004a3b2c1c0"),
            ],
            Some(BOOT_A),
        );
        assert_eq!(amd_ce.len(), 1);
        assert_eq!(
            amd_ce[0].severity,
            Severity::Medium,
            "detail lines do not make a corrected error worse"
        );
        assert_eq!(amd_ce[0].count, 3);

        let fatal = run(
            &[
                kernel_at(
                    1,
                    BOOT_A,
                    "4",
                    "pcieport 0000:00:1c.0: AER: PCIe Bus Error: severity=Correctable, type=Physical Layer, (Receiver ID)",
                ),
                kernel_at(
                    2,
                    BOOT_A,
                    "3",
                    "pcieport 0000:00:1c.0: AER: PCIe Bus Error: severity=Uncorrectable (Fatal), type=Transaction Layer, (Requester ID)",
                ),
            ],
            Some(BOOT_A),
        );
        assert_eq!((fatal.len(), fatal[0].severity), (1, Severity::High));
        assert_eq!(fatal[0].title, "PCIe AER errors (2)");

        let ue = run(
            &[kernel_at(1, BOOT_A, "0", "EDAC MC0: 1 UE memory read error on DIMM0")],
            Some(BOOT_A),
        );
        assert_eq!(ue[0].severity, Severity::High);
    }

    #[test]
    fn gpu_faults_from_every_vendor_are_hardware_errors() {
        let cases = [
            // NVIDIA open-gpu-kernel-modules: "NVRM: Xid (PCI:%04x:%02x:%02x): %d, %s"
            (
                "3",
                "NVRM: Xid (PCI:0000:01:00): 79, pid=1234, name=Xorg, GPU has fallen off the bus.",
                "NVIDIA GPU errors (Xid)",
            ),
            // drivers/gpu/drm/amd/amdgpu/amdgpu_job.c: "ring %s timeout, signaled seq=%u, emitted seq=%u"
            (
                "3",
                "amdgpu 0000:03:00.0: [drm] *ERROR* ring gfx_0.0.0 timeout, signaled seq=1234, emitted seq=1236",
                "AMD GPU hangs",
            ),
            // drivers/gpu/drm/amd/amdgpu/amdgpu_device.c: dev_info "GPU reset begin!"
            ("6", "amdgpu 0000:03:00.0: amdgpu: GPU reset begin!", "AMD GPU hangs"),
            // drivers/gpu/drm/i915/i915_gpu_error.c: "GPU HANG: ecode %d:%x:%08x"
            (
                "6",
                "i915 0000:00:02.0: [drm] GPU HANG: ecode 12:1:85dffffb, in Xorg [1234]",
                "Intel GPU hangs",
            ),
            // drivers/gpu/drm/xe/xe_guc_submit.c: xe_gt_notice "Timedout job: seqno=%u, lrc_seqno=%u, guc_id=%d, flags=0x%lx"
            (
                "5",
                "xe 0000:00:02.0: [drm] GT0: Timedout job: seqno=4294967169, lrc_seqno=4294967169, guc_id=2, flags=0x0 in gnome-shell [2034]",
                "Intel GPU hangs",
            ),
        ];
        for (priority, line, what) in cases {
            let f = run(&[kernel_at(1, BOOT_A, priority, line)], Some(BOOT_A));
            assert_eq!(f.len(), 1, "{line}");
            assert_eq!((f[0].kind, f[0].severity), (FindingKind::HardwareError, Severity::High));
            assert_eq!(f[0].data["error"], what);
        }
    }

    #[test]
    fn storage_timeouts_and_errors_are_found_but_empty_drives_and_loop_files_are_not() {
        let cases = [
            // drivers/nvme/host/pci.c
            (
                "4",
                "nvme nvme0: I/O tag 12 (100c) QID 3 timeout, aborting req_op:READ(0) size:4096",
                "NVMe timeouts",
            ),
            (
                "4",
                "nvme nvme0: controller is down; will reset: CSTS=0xffffffff, PCI_STATUS=0xffff",
                "NVMe timeouts",
            ),
            // drivers/ata/libata-eh.c
            (
                "3",
                "ata1.00: exception Emask 0x0 SAct 0x10 SErr 0x0 action 0x0",
                "SATA errors",
            ),
            ("3", "ata1.00: failed command: READ FPDMA QUEUED", "SATA errors"),
            // block/blk-core.c blk_print_req_error, with a blk_errors[] name other than "I/O"
            (
                "3",
                "critical medium error, dev sdb, sector 2048 op 0x0:(READ) flags 0x80700 phys_seg 1 prio class 2",
                "Disk I/O errors",
            ),
            (
                "3",
                "timeout error, dev nvme0n1, sector 4096 op 0x1:(WRITE) flags 0x0 phys_seg 1 prio class 2",
                "Disk I/O errors",
            ),
            // fs/buffer.c
            (
                "3",
                "Buffer I/O error on dev sda1, logical block 0, async page read",
                "Disk I/O errors",
            ),
        ];
        for (priority, line, what) in cases {
            let f = run(&[kernel_at(1, BOOT_A, priority, line)], Some(BOOT_A));
            assert_eq!(f.len(), 1, "{line}");
            assert_eq!(f[0].data["error"], what, "{line}");
        }
        for line in [
            "I/O error, dev fd0, sector 0 op 0x0:(READ) flags 0x0 phys_seg 1 prio class 2",
            "I/O error, dev sr0, sector 0 op 0x0:(READ) flags 0x80700 phys_seg 1 prio class 2",
            "I/O error, dev loop3, sector 0 op 0x0:(READ) flags 0x0 phys_seg 1 prio class 2",
            "Buffer I/O error on dev sr0, logical block 0, async page read",
        ] {
            assert!(
                run(&[kernel_at(1, BOOT_A, "3", line)], Some(BOOT_A)).is_empty(),
                "{line}"
            );
        }
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
                (
                    "MESSAGE",
                    "pam_unix(sudo:auth): authentication failure; logname=alice uid=1000 euid=0 tty=/dev/pts/1 ruser=alice rhost=  user=alice",
                ),
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
            boot_start(1, BOOT_A),
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
        manager(
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

    // arch/x86/mm/fault.c show_signal_msg: "%s%s[%d]: segfault at %lx ip %px sp %px error %lx"
    const SEGFAULT_LINE: &str = "Hyprland[4242]: segfault at 0 ip 00007fef17a2d5e1 sp 00007fffd7cfb4e0 error 4 in libfoo.so.0[bc5e1,7fef1799c000+e3000] likely on CPU 3 (core 3, socket 0)";

    #[test]
    fn a_kernel_segfault_with_no_core_dump_is_a_crash_so_systems_without_systemd_coredump_still_show_it() {
        let f = run(
            &[kernel_at(10, BOOT_A, "6", SEGFAULT_LINE), noise(100, BOOT_A)],
            Some(BOOT_A),
        );
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].kind, f[0].severity), (FindingKind::Crash, Severity::High));
        assert_eq!(f[0].title, "Hyprland crashed (SIGSEGV)");
        assert_eq!(
            (f[0].data["pid"].as_str(), f[0].data["source"].as_str()),
            ("4242", "kernel")
        );
        assert_eq!((f[0].first_us, f[0].entries.clone()), (10 * S, vec![0]));
    }

    #[test]
    fn a_traps_line_names_its_signal() {
        // arch/x86/kernel/traps.c do_trap: pr_info("%s[%d] trap %s ip:%lx sp:%lx error:%lx") under pr_fmt "traps: "
        let ill =
            "traps: game[777] trap invalid opcode ip:55d0c1e0a1b2 sp:7ffd1e8b8c70 error:0 in game[55d0c1e00000+20000]";
        let gp = "traps: tool[778] general protection fault ip:7f00 sp:7ffd error:0 in libc.so.6[7f00+1000]";
        let f = run(&[kernel(10, BOOT_A, ill), kernel(11, BOOT_A, gp)], None);
        let got: Vec<_> = f.iter().map(|f| f.title.as_str()).collect();
        assert_eq!(got, ["game crashed (SIGILL)", "tool crashed (SIGSEGV)"]);
        assert!(
            run(
                &[kernel(1, BOOT_A, "traps: PANIC: double fault, error_code: 0x0")],
                None
            )
            .is_empty()
        );
    }

    #[test]
    fn a_segfault_and_its_core_dump_are_one_crash_in_either_order() {
        let dump = coredump(12, BOOT_A, "Hyprland");
        let f = run(&[kernel(10, BOOT_A, SEGFAULT_LINE), dump.clone()], Some(BOOT_A));
        assert_eq!(f.len(), 1, "{f:#?}");
        assert_eq!(
            f[0].evidence, "Process 4242 (Hyprland) of user 1000 dumped core.",
            "the richer report wins"
        );
        let f = run(&[dump, kernel(13, BOOT_A, SEGFAULT_LINE)], Some(BOOT_A));
        assert_eq!(f.len(), 1);
        let other_pid = SEGFAULT_LINE.replace("4242", "5151");
        let f = run(
            &[kernel(10, BOOT_A, &other_pid), coredump(12, BOOT_A, "Hyprland")],
            Some(BOOT_A),
        );
        assert_eq!(f.len(), 2, "another process's crash is not deduplicated");
    }

    #[test]
    fn a_followed_kernel_crash_waits_for_its_core_dump_then_comes_out_once() {
        let mut d = Detector::new();
        d.push(0, &cursored(kernel(10, BOOT_A, SEGFAULT_LINE), 0));
        assert!(d.drain(15 * S).is_empty(), "a core dump may still follow");
        assert_eq!(d.open_since(), Some(10 * S), "a resumed reader must see the line again");
        let f = d.drain(10 * S + CRASH_DEDUP_US + 1);
        assert_eq!((f.len(), f[0].cursor.as_deref()), (1, Some("c0")));
        assert!(d.drain(100 * S).is_empty());
        assert_eq!(d.open_since(), None);
    }

    #[test]
    fn crash_ordinals_read_correctly_past_twenty() {
        let es: Vec<_> = (0..111).map(|i| coredump(i, BOOT_A, "x")).collect();
        let f = run(&es, Some(BOOT_A));
        for (n, want) in [
            (21, "21st"),
            (22, "22nd"),
            (23, "23rd"),
            (11, "11th"),
            (12, "12th"),
            (13, "13th"),
            (111, "111th"),
        ] {
            assert_eq!(f[n - 1].title, format!("{want} x crash this boot (SIGSEGV)"));
        }
    }

    #[test]
    fn the_kernel_kill_of_oom_kill_allocating_task_and_its_summary_are_found() {
        // mm/oom_kill.c: "%s: Killed process %d (%s)" with "Out of memory (oom_kill_allocating_task)"
        let f = run(
            &[
                kernel(
                    100,
                    BOOT_A,
                    "oom-kill:constraint=CONSTRAINT_NONE,nodemask=(null),cpuset=/,mems_allowed=0,global_oom,task_memcg=/user.slice,task=big,pid=88,uid=1000",
                ),
                kernel(
                    100,
                    BOOT_A,
                    "Out of memory (oom_kill_allocating_task): Killed process 88 (big) total-vm:9000kB, anon-rss:8000kB",
                ),
            ],
            Some(BOOT_A),
        );
        assert_eq!(f.len(), 1);
        assert_eq!(
            (f[0].count, f[0].data["pid"].as_str(), f[0].title.as_str()),
            (2, "88", "big was killed by the OOM killer")
        );
    }

    fn forged(mut e: Entry, pid: &str, uid: &str, comm: &str) -> Entry {
        for (k, v) in [("_PID", pid), ("_UID", uid), ("_COMM", comm)] {
            e.fields.insert(k.into(), v.into());
        }
        e
    }

    #[test]
    fn a_forged_unit_failure_or_restart_loop_from_another_uid_is_ignored() {
        let failure = unit_failed(10, BOOT_A);
        let mut d = Detector::for_owner(1000);
        d.push(0, &forged(failure.clone(), "4242", "1001", "systemd"));
        d.push(1, &forged(failure.clone(), "4243", "1000", "python3"));
        for (i, t) in [20, 30, 40].into_iter().enumerate() {
            d.push(2 + i, &forged(restart(t, "x.service"), "4242", "1001", "systemd"));
        }
        assert!(
            d.finish(Some(BOOT_A)).is_empty(),
            "another user, or a process merely claiming the unit"
        );

        let mut d = Detector::for_owner(1000);
        d.push(0, &failure);
        d.push(1, &forged(unit_failed(20, BOOT_A), "2011", "1000", "systemd"));
        let f = d.finish(Some(BOOT_A));
        assert_eq!(
            (f.len(), f[0].count),
            (1, 2),
            "PID 1 and the owner's user manager are believed"
        );
    }

    #[test]
    fn only_pid_1_as_root_or_the_owners_user_manager_is_trusted() {
        let e = |fields: &[(&str, &str)]| ev(1, BOOT_A, fields);
        assert!(trusted_manager_entry(&e(PID1), 1000));
        assert!(
            !trusted_manager_entry(&e(&[("_PID", "1"), ("_UID", "1000"), ("_COMM", "systemd")]), 1000),
            "PID 1 of a user namespace"
        );
        assert!(trusted_manager_entry(
            &e(&[("_PID", "900"), ("_UID", "1000"), ("_COMM", "systemd")]),
            1000
        ));
        assert!(!trusted_manager_entry(
            &e(&[("_PID", "900"), ("_UID", "1001"), ("_COMM", "systemd")]),
            1000
        ));
        assert!(!trusted_manager_entry(
            &e(&[("_PID", "900"), ("_UID", "1000"), ("_COMM", "bash")]),
            1000
        ));
        assert!(
            !trusted_manager_entry(&e(&[("UNIT", "x.service")]), 1000),
            "no trusted fields at all"
        );
    }

    #[test]
    fn only_ordinary_services_sockets_and_mounts_are_restartable() {
        for ok in [
            "sshd.service",
            "cups.socket",
            "home.mount",
            "getty@tty1.service",
            "foo\\x2dbar.service",
            "a:b_c.service",
        ] {
            assert!(restartable_unit(ok), "{ok}");
        }
        for bad in [
            "--host=attacker",
            "-x.service",
            "--host=a.service",
            "user@1000.service",
            "user-runtime-dir@1000.service",
            "session-3.scope",
            "user.slice",
            "multi-user.target",
            "backup.timer",
            "gdm.service",
            "gdm3.service",
            "sddm.service",
            "lightdm.service",
            "greetd.service",
            "ly.service",
            "display-manager.service",
            "getty@.service",
            "@x.service",
            ".service",
            "a b.service",
            "x.service; rm",
            "",
        ] {
            assert!(!restartable_unit(bad), "{bad}");
        }
        assert!(!restartable_unit(&format!("{}.service", "a".repeat(250))));
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
