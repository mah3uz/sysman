use std::io;
use std::process::{Command, Stdio};

use schema::Sample;
use schema::helper::Helper;
use schema::processes::Process;
use schema::storage::Smart;

use crate::charts::format::bytes;

pub const UNIT: &str = "sysman-helper.service";

/// What the helper adds, in the order Settings lists it: feature key and label.
pub const FEATURES: [(&str, &str); 4] = [
    ("smart", "Drive health (SMART)"),
    ("rapl", "CPU package power"),
    ("proc_io", "Disk I/O and open files of other users' processes"),
    ("net", "Per-app network traffic"),
];

/// A finished command's outcome.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ran {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a program to completion; tests put a fake in place of `systemctl`.
pub trait Runner: Send + Sync {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Ran>;
}

pub struct System;

impl Runner for System {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<Ran> {
        // No stdin, so systemctl asks the desktop's polkit agent rather than a terminal.
        let out = Command::new(program).args(args).stdin(Stdio::null()).output()?;
        Ok(Ran {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

pub fn toggle_args(on: bool) -> [&'static str; 3] {
    [if on { "enable" } else { "disable" }, "--now", UNIT]
}

/// The service as systemd sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    pub installed: bool,
    pub enabled: bool,
    /// `is-active` output: `active`, `inactive`, `failed`, `activating`, …
    pub active: String,
}

impl Unit {
    /// Where the toggle sits: on once enabled or started, whichever systemd reports first.
    pub fn on(&self) -> bool {
        self.enabled || matches!(self.active.as_str(), "active" | "activating")
    }
}

/// Older systemd prints nothing on stdout for a missing unit and says so on stderr.
pub fn parse_unit(is_enabled: &Ran, is_active: &Ran) -> Unit {
    let enabled = is_enabled.stdout.trim();
    let missing = enabled == "not-found"
        || (enabled.is_empty()
            && (is_enabled.stderr.contains("No such file") || is_enabled.stderr.contains("not found")));
    Unit {
        installed: !missing,
        enabled: matches!(enabled, "enabled" | "enabled-runtime"),
        active: is_active.stdout.trim().to_owned(),
    }
}

pub fn query(runner: &dyn Runner) -> Result<Unit, String> {
    let run = |verb: &str| {
        runner
            .run("systemctl", &[verb, UNIT])
            .map_err(|e| format!("Could not run systemctl: {e}"))
    };
    Ok(parse_unit(&run("is-enabled")?, &run("is-active")?))
}

/// `enable --now` or `disable --now`; systemd asks polkit, which shows the system's authentication dialog.
pub fn set(runner: &dyn Runner, on: bool) -> Result<(), String> {
    let ran = runner
        .run("systemctl", &toggle_args(on))
        .map_err(|e| format!("Could not run systemctl: {e}"))?;
    if ran.ok {
        return Ok(());
    }
    let message = ran.stderr.trim();
    Err(if message.is_empty() {
        format!("systemctl {} failed", toggle_args(on).join(" "))
    } else {
        message.to_owned()
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    NotInstalled,
    Off,
    /// The service runs but sysmand has not reported it yet; it looks every 10 s.
    Starting,
    Running,
    Error(String),
}

impl State {
    pub fn label(&self) -> &'static str {
        match self {
            Self::NotInstalled => "Not installed",
            Self::Off => "Off",
            Self::Starting => "Starting",
            Self::Running => "Running",
            Self::Error(_) => "Error",
        }
    }
}

/// The daemon's `helper` module wins; systemd's view tells "off" from "not installed" and catches a failed unit.
pub fn state(module: Option<&Helper>, unit: Option<&Unit>) -> State {
    match module.map(|m| m.state.as_str()) {
        Some("running") => return State::Running,
        Some("error") => {
            let message = module.and_then(|m| m.error.clone());
            return State::Error(message.unwrap_or_else(|| "The helper reported an error".into()));
        }
        _ => {}
    }
    match unit {
        Some(u) if !u.installed => State::NotInstalled,
        Some(u) if u.active == "failed" => State::Error(format!("{UNIT} failed; journalctl -u {UNIT} has the details")),
        Some(u) if matches!(u.active.as_str(), "active" | "activating") => State::Starting,
        _ => State::Off,
    }
}

/// A daemon without the `helper` module is treated as reporting `stopped`.
pub fn running(sample: Option<&Sample>) -> bool {
    sample
        .and_then(|s| s.helper.as_ref())
        .is_some_and(|h| h.state == "running")
}

/// Each feature with whether it works; all off unless the helper runs.
pub fn features(module: Option<&Helper>) -> [(&'static str, bool); 4] {
    FEATURES.map(|(key, label)| {
        let works = module.is_some_and(|m| m.state == "running" && m.features.get(key) == Some(&true));
        (label, works)
    })
}

/// The Disk tab's Health rail below the btrfs counters.
#[derive(Debug, PartialEq)]
pub enum DiskHealth<'a> {
    Smart(&'a Smart),
    Hint,
    /// The helper runs but has no reading for this drive (asleep, or a bridge that hides SMART).
    NoReading,
}

pub fn disk_health<'a>(sample: Option<&'a Sample>, disk: Option<&str>) -> DiskHealth<'a> {
    let smart = disk.and_then(|d| sample?.storage.as_ref()?.smart.get(d));
    match smart {
        Some(s) => DiskHealth::Smart(s),
        None if running(sample) => DiskHealth::NoReading,
        None => DiskHealth::Hint,
    }
}

/// The Processes table's Net columns: only while the helper runs and some row carries a rate.
pub fn net_columns(rows: &[Process], helper_running: bool) -> bool {
    helper_running && rows.iter().any(|p| p.net_rx_bps.is_some() || p.net_tx_bps.is_some())
}

/// Busiest first by download plus upload; idle processes are left out.
pub fn top_traffic(rows: &[Process], n: usize) -> Vec<&Process> {
    let total = |p: &Process| p.net_rx_bps.unwrap_or(0) + p.net_tx_bps.unwrap_or(0);
    let mut busy: Vec<&Process> = rows.iter().filter(|p| total(p) > 0).collect();
    busy.sort_by(|a, b| total(b).cmp(&total(a)).then(a.pid.cmp(&b.pid)));
    busy.truncate(n);
    busy
}

pub fn hours_as_days(hours: u64) -> String {
    if hours < 48 {
        return format!("{hours} h");
    }
    let days = hours / 24;
    if days < 730 {
        format!("{days} days")
    } else {
        format!("{:.1} years ({days} days)", days as f64 / 365.25)
    }
}

/// The SMART block's rows below the verdict and the life-used bar.
pub fn smart_rows(s: &Smart) -> [(&'static str, String); 7] {
    let count = |v: Option<u64>| v.map_or("–".into(), |n| n.to_string());
    let size = |v: Option<u64>| v.map_or("–".into(), |b| bytes(b as f64));
    [
        (
            "Available spare",
            s.available_spare_pct.map_or("–".into(), |p| format!("{p}%")),
        ),
        ("Power-on time", s.power_on_hours.map_or("–".into(), hours_as_days)),
        ("Power cycles", count(s.power_cycles)),
        ("Unsafe shutdowns", count(s.unsafe_shutdowns)),
        ("Media errors", count(s.media_errors)),
        ("Data read", size(s.data_read_bytes)),
        ("Data written", size(s.data_written_bytes)),
    ]
}

/// Life used: green, orange from 80%, red from 100% where the vendor's endurance estimate is spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wear {
    Fine,
    Worn,
    Spent,
}

pub fn wear(percent_used: u8) -> Wear {
    match percent_used {
        0..80 => Wear::Fine,
        80..100 => Wear::Worn,
        _ => Wear::Spent,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;

    fn ran(ok: bool, stdout: &str, stderr: &str) -> Ran {
        Ran {
            ok,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    /// Answers by verb and records every call, standing in for systemctl.
    struct Fake {
        answers: Vec<(&'static str, Ran)>,
        calls: Mutex<Vec<String>>,
    }

    impl Runner for Fake {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<Ran> {
            self.calls.lock().unwrap().push(format!("{program} {}", args.join(" ")));
            self.answers
                .iter()
                .find(|(verb, _)| *verb == args[0])
                .map(|(_, r)| r.clone())
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such program"))
        }
    }

    fn fake(answers: Vec<(&'static str, Ran)>) -> Fake {
        Fake {
            answers,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn module(state: &str) -> Helper {
        Helper {
            state: state.into(),
            version: None,
            features: BTreeMap::new(),
            error: None,
        }
    }

    fn unit(installed: bool, enabled: bool, active: &str) -> Unit {
        Unit {
            installed,
            enabled,
            active: active.into(),
        }
    }

    #[test]
    fn systemd_tells_not_installed_from_installed_but_off() {
        let missing = parse_unit(&ran(false, "not-found\n", ""), &ran(false, "inactive\n", ""));
        assert!(!missing.installed);
        let old_systemd = parse_unit(
            &ran(
                false,
                "",
                "Failed to get unit file state for sysman-helper.service: No such file or directory",
            ),
            &ran(false, "inactive", ""),
        );
        assert!(
            !old_systemd.installed,
            "older systemd reports a missing unit on stderr only"
        );
        let off = parse_unit(&ran(false, "disabled\n", ""), &ran(false, "inactive\n", ""));
        assert_eq!(off, unit(true, false, "inactive"));
        assert!(!off.on());
        let on = parse_unit(&ran(true, "enabled\n", ""), &ran(true, "active\n", ""));
        assert!(on.installed && on.enabled && on.on());
    }

    #[test]
    fn the_state_comes_from_the_module_first_then_systemd() {
        assert_eq!(state(None, Some(&unit(false, false, "inactive"))), State::NotInstalled);
        assert_eq!(state(None, Some(&unit(true, false, "inactive"))), State::Off);
        assert_eq!(
            state(Some(&module("stopped")), Some(&unit(true, true, "active"))),
            State::Starting,
            "the service is up but sysmand has not seen it yet"
        );
        assert_eq!(
            state(None, None),
            State::Off,
            "a daemon without the module means stopped"
        );
        assert_eq!(state(Some(&module("running")), None), State::Running);
        let mut broken = module("error");
        broken.error = Some("permission denied on helper.sock".into());
        assert_eq!(
            state(Some(&broken), Some(&unit(true, true, "active"))),
            State::Error("permission denied on helper.sock".into())
        );
        assert!(matches!(
            state(Some(&module("stopped")), Some(&unit(true, true, "failed"))),
            State::Error(m) if m.contains("journalctl")
        ));
    }

    #[test]
    fn the_toggle_runs_enable_or_disable_now_on_the_helper_unit_only() {
        assert_eq!(toggle_args(true), ["enable", "--now", "sysman-helper.service"]);
        assert_eq!(toggle_args(false), ["disable", "--now", "sysman-helper.service"]);
        let runner = fake(vec![("enable", ran(true, "", ""))]);
        assert_eq!(set(&runner, true), Ok(()));
        assert_eq!(
            *runner.calls.lock().unwrap(),
            ["systemctl enable --now sysman-helper.service"]
        );
    }

    #[test]
    fn a_failed_toggle_reports_what_systemctl_said() {
        let cancelled = fake(vec![(
            "enable",
            ran(false, "", "Failed to enable unit: Access denied\n"),
        )]);
        assert_eq!(
            set(&cancelled, true),
            Err("Failed to enable unit: Access denied".into())
        );
        let silent = fake(vec![("disable", ran(false, "", ""))]);
        assert_eq!(
            set(&silent, false),
            Err("systemctl disable --now sysman-helper.service failed".into())
        );
        let absent = fake(vec![]);
        assert!(set(&absent, true).unwrap_err().starts_with("Could not run systemctl"));
    }

    #[test]
    fn query_asks_is_enabled_and_is_active() {
        let runner = fake(vec![
            ("is-enabled", ran(false, "disabled\n", "")),
            ("is-active", ran(false, "inactive\n", "")),
        ]);
        assert_eq!(query(&runner), Ok(unit(true, false, "inactive")));
        assert_eq!(
            *runner.calls.lock().unwrap(),
            [
                "systemctl is-enabled sysman-helper.service",
                "systemctl is-active sysman-helper.service"
            ]
        );
    }

    #[test]
    fn features_are_ticked_only_while_the_helper_runs() {
        let mut m = module("running");
        m.features = BTreeMap::from([("smart".into(), true), ("net".into(), false)]);
        let f = features(Some(&m));
        assert_eq!(f[0], ("Drive health (SMART)", true));
        assert!(!f[1].1, "a feature the helper does not report is not offered");
        assert!(!f[3].1);
        m.state = "error".into();
        assert!(features(Some(&m)).iter().all(|(_, works)| !works));
    }

    fn sample(helper: Option<&str>, smart: &[&str]) -> Sample {
        let mut s: Sample = serde_json::from_value(serde_json::json!({
            "type": "sample", "schema": 3, "t": 1.0, "time": "x",
            "storage": {"devices": {}, "btrfs": {}}
        }))
        .unwrap();
        s.helper = helper.map(module);
        let storage = s.storage.as_mut().unwrap();
        for d in smart {
            storage.smart.insert((*d).into(), Smart::default());
        }
        s
    }

    #[test]
    fn hints_show_only_while_the_helper_is_not_running() {
        assert!(!running(None));
        assert!(
            !running(Some(&sample(None, &[]))),
            "an older daemon has no helper module"
        );
        assert!(!running(Some(&sample(Some("error"), &[]))));
        assert!(running(Some(&sample(Some("running"), &[]))));

        let off = sample(None, &[]);
        assert_eq!(disk_health(Some(&off), Some("nvme0n1")), DiskHealth::Hint);
        let on = sample(Some("running"), &["nvme0n1"]);
        assert!(matches!(disk_health(Some(&on), Some("nvme0n1")), DiskHealth::Smart(_)));
        assert_eq!(
            disk_health(Some(&on), Some("sda")),
            DiskHealth::NoReading,
            "no hint to enable a helper that already runs"
        );
    }

    fn proc(pid: u32, rx: Option<u64>, tx: Option<u64>) -> Process {
        Process {
            pid,
            ppid: 1,
            comm: format!("p{pid}"),
            state: "S".into(),
            threads: 1,
            rss_bytes: 0,
            cpu_pct: 0.,
            read_bps: None,
            write_bps: None,
            user: None,
            fds: None,
            net_rx_bps: rx,
            net_tx_bps: tx,
            cmdline: String::new(),
            start_time: 0,
        }
    }

    #[test]
    fn net_columns_need_the_helper_and_a_row_with_rates() {
        let quiet = [proc(1, None, None)];
        let busy = [proc(1, None, None), proc(2, Some(0), None)];
        assert!(!net_columns(&busy, false));
        assert!(!net_columns(&quiet, true), "no column of dashes when no row has a rate");
        assert!(net_columns(&busy, true));
    }

    #[test]
    fn per_app_traffic_ranks_by_download_plus_upload() {
        let rows = [
            proc(1, Some(10), Some(10)),
            proc(2, Some(100), None),
            proc(3, None, None),
            proc(4, Some(0), Some(0)),
            proc(5, None, Some(5)),
        ];
        let top: Vec<u32> = top_traffic(&rows, 2).iter().map(|p| p.pid).collect();
        assert_eq!(top, [2, 1]);
        assert_eq!(top_traffic(&rows, 10).len(), 3, "idle processes are left out");
    }

    #[test]
    fn power_on_hours_read_as_days() {
        assert_eq!(hours_as_days(5), "5 h");
        assert_eq!(hours_as_days(47), "47 h");
        assert_eq!(hours_as_days(12_345), "514 days");
        assert_eq!(hours_as_days(24 * 1000), "2.7 years (1000 days)");
    }

    #[test]
    fn smart_rows_show_sizes_in_binary_units_and_dashes_for_what_the_drive_omits() {
        let s = Smart {
            passed: true,
            power_on_hours: Some(12_345),
            data_written_bytes: Some(3 * 1024u64.pow(4)),
            available_spare_pct: Some(100),
            ..Smart::default()
        };
        let rows = smart_rows(&s);
        let get = |label: &str| rows.iter().find(|(l, _)| *l == label).unwrap().1.clone();
        assert_eq!(get("Data written"), "3.0 TB");
        assert_eq!(get("Data read"), "–");
        assert_eq!(get("Power-on time"), "514 days");
        assert_eq!(get("Available spare"), "100%");
        assert_eq!(get("Media errors"), "–");
    }

    #[test]
    fn life_used_turns_orange_at_80_and_red_once_spent() {
        assert_eq!(wear(0), Wear::Fine);
        assert_eq!(wear(79), Wear::Fine);
        assert_eq!(wear(80), Wear::Worn);
        assert_eq!(wear(99), Wear::Worn);
        assert_eq!(wear(100), Wear::Spent);
        assert_eq!(wear(255), Wear::Spent, "the vendor estimate may exceed 100");
    }
}
