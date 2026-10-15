use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::MetadataExt;

use anyhow::{Context, Result, bail};
use schema::processes::{Group, Process, Table};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::collector::Collector;
use crate::deltas::Deltas;
use crate::sys::Sys;

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Options {
    pub top_n: usize,
    pub io: bool,
    pub groups: bool,
    pub cmdline_max: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            top_n: 10,
            io: true,
            groups: true,
            cmdline_max: 200,
        }
    }
}

struct Snapshot {
    pid: u32,
    ppid: u32,
    comm: String,
    state: String,
    threads: u64,
    rss_bytes: u64,
    cmdline: String,
    user: Option<String>,
    fds: Option<u64>,
}

/// Scans `/proc` and turns cumulative counters into rates. It owns its rate
/// state, so scanners used by different callers do not disturb each other.
pub struct ProcessScanner {
    sys: Sys,
    io: bool,
    cmdline_max: usize,
    clk_tck: f64,
    page_size: u64,
    deltas: Deltas<String>,
}

impl ProcessScanner {
    pub fn new(sys: Sys, io: bool, cmdline_max: usize) -> Result<Self> {
        // SAFETY: sysconf has no preconditions.
        let (clk_tck, page_size) = unsafe { (libc::sysconf(libc::_SC_CLK_TCK), libc::sysconf(libc::_SC_PAGESIZE)) };
        if clk_tck <= 0 || page_size <= 0 {
            bail!("sysconf could not report the clock tick rate or page size");
        }
        Ok(Self {
            sys,
            io,
            cmdline_max,
            clk_tck: clk_tck as f64,
            page_size: page_size as u64,
            deltas: Deltas::default(),
        })
    }

    /// `None` on the first call, which only primes the counters.
    pub fn scan(&mut self) -> Result<Option<Table>> {
        let users = self.users();
        let mut procs: HashMap<String, Snapshot> = HashMap::new();
        let mut counters: HashMap<String, Vec<u64>> = HashMap::new();
        for pid in self.sys.list("/proc") {
            if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            // the process may exit between the listing and any of these reads
            let Some(stat) = self.sys.read_opt(format!("/proc/{pid}/stat")) else {
                continue;
            };
            let (snap, start, ticks) = self.parse_stat(&pid, &stat, &users)?;
            let mut counts = vec![ticks];
            if self.io
                && let Some(io) = self.sys.read_opt(format!("/proc/{pid}/io"))
            {
                counts.extend(parse_io(&io));
            }
            let key = format!("{pid}:{start}");
            procs.insert(key.clone(), snap);
            counters.insert(key, counts);
        }

        let Some((dt, deltas)) = self.deltas.update(counters) else {
            return Ok(None);
        };

        let mut states = BTreeMap::new();
        let mut threads = 0;
        let mut rows = Vec::new();
        let count = procs.len() as u64;
        for (key, p) in procs {
            *states.entry(p.state.clone()).or_insert(0) += 1;
            threads += p.threads;
            let Some(d) = deltas.get(&key) else { continue };
            let io = (d.len() == 3).then(|| ((d[1] as f64 / dt).round() as u64, (d[2] as f64 / dt).round() as u64));
            rows.push(Process {
                pid: p.pid,
                ppid: p.ppid,
                comm: p.comm,
                state: p.state,
                threads: p.threads,
                rss_bytes: p.rss_bytes,
                cpu_pct: round1(100.0 * d[0] as f64 / self.clk_tck / dt),
                read_bps: io.map(|i| i.0),
                write_bps: io.map(|i| i.1),
                user: p.user,
                fds: p.fds,
                cmdline: p.cmdline,
            });
        }
        rows.sort_by_key(|r| r.pid);
        Ok(Some(Table {
            count,
            threads,
            states,
            rows,
        }))
    }

    fn parse_stat(&self, pid: &str, stat: &str, users: &HashMap<u32, String>) -> Result<(Snapshot, String, u64)> {
        // comm may contain spaces and parentheses, so split on the last ")"
        let (head, tail) = stat
            .rsplit_once(')')
            .with_context(|| format!("/proc/{pid}/stat: no closing parenthesis"))?;
        let comm = head.split_once('(').map_or("", |(_, c)| c).to_string();
        let f: Vec<&str> = tail.split_whitespace().collect();
        if f.len() < 22 {
            bail!("/proc/{pid}/stat: {} fields after comm, expected at least 22", f.len());
        }
        let num = |i: usize| -> Result<u64> {
            f[i].parse()
                .with_context(|| format!("/proc/{pid}/stat: field {:?}", f[i]))
        };
        let uid = std::fs::metadata(self.sys.path(format!("/proc/{pid}")))
            .ok()
            .map(|m| m.uid());
        let fds = std::fs::read_dir(self.sys.path(format!("/proc/{pid}/fd")))
            .ok()
            .map(|d| d.count() as u64);
        let cmdline = self
            .sys
            .read_opt(format!("/proc/{pid}/cmdline"))
            .unwrap_or_default()
            .replace('\0', " ");
        let snap = Snapshot {
            pid: pid.parse().with_context(|| format!("pid {pid:?}"))?,
            ppid: num(1)? as u32,
            comm,
            state: f[0].to_string(),
            threads: num(17)?,
            rss_bytes: num(21)? * self.page_size,
            cmdline: cmdline.trim().chars().take(self.cmdline_max).collect(),
            user: uid.map(|u| users.get(&u).cloned().unwrap_or_else(|| u.to_string())),
            fds,
        };
        Ok((snap, f[19].to_string(), num(11)? + num(12)?))
    }

    fn users(&self) -> HashMap<u32, String> {
        let mut users = HashMap::new();
        for line in self.sys.read_opt("/etc/passwd").unwrap_or_default().lines() {
            let mut f = line.split(':');
            if let (Some(name), Some(_), Some(uid)) = (f.next(), f.next(), f.next())
                && let Ok(uid) = uid.parse()
            {
                users.entry(uid).or_insert_with(|| name.to_string());
            }
        }
        users
    }
}

fn parse_io(text: &str) -> Vec<u64> {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(": ")?.trim().parse().ok())
    };
    match (field("read_bytes"), field("write_bytes")) {
        (Some(r), Some(w)) => vec![r, w],
        _ => Vec::new(),
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn top<T: Clone>(items: &[T], key: impl Fn(&T) -> f64, n: usize) -> Vec<T> {
    let mut v: Vec<&T> = items.iter().filter(|i| key(i) > 0.0).collect();
    v.sort_by(|a, b| key(b).total_cmp(&key(a)));
    v.into_iter().take(n).cloned().collect()
}

pub struct Processes {
    scanner: ProcessScanner,
    opts: Options,
}

impl Processes {
    pub fn new(opts: Options, sys: Sys) -> Result<Self> {
        Ok(Self {
            scanner: ProcessScanner::new(sys, opts.io, opts.cmdline_max)?,
            opts,
        })
    }
}

impl Collector for Processes {
    fn meta(&mut self) -> Option<Value> {
        Some(json!({
            "cpu_pct": "percent of one core",
            "io": "own processes only; other users' processes need root",
            "groups": "processes summed by name; shared memory is counted once per process",
        }))
    }

    fn sample(&mut self) -> Result<Option<Value>> {
        let Some(table) = self.scanner.scan()? else {
            return Ok(None);
        };
        let n = self.opts.top_n;
        let rows = &table.rows;
        let mut out = schema::processes::Processes {
            count: table.count,
            threads: table.threads,
            states: table.states.clone(),
            top_cpu: top(rows, |r| r.cpu_pct, n),
            top_memory: top(rows, |r| r.rss_bytes as f64, n),
            top_io: None,
            groups_cpu: None,
            groups_memory: None,
        };
        if self.opts.io {
            let io = |r: &Process| (r.read_bps.unwrap_or(0) + r.write_bps.unwrap_or(0)) as f64;
            out.top_io = Some(top(rows, io, n));
        }
        if self.opts.groups {
            let mut groups: BTreeMap<&str, Group> = BTreeMap::new();
            for r in rows {
                let g = groups.entry(&r.comm).or_insert_with(|| Group {
                    name: r.comm.clone(),
                    count: 0,
                    cpu_pct: 0.0,
                    rss_bytes: 0,
                });
                g.count += 1;
                g.cpu_pct += r.cpu_pct;
                g.rss_bytes += r.rss_bytes;
            }
            let groups: Vec<Group> = groups
                .into_values()
                .map(|g| Group {
                    cpu_pct: round1(g.cpu_pct),
                    ..g
                })
                .collect();
            out.groups_cpu = Some(top(&groups, |g| g.cpu_pct, n));
            out.groups_memory = Some(top(&groups, |g| g.rss_bytes as f64, n));
        }
        Ok(Some(serde_json::to_value(out)?))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    // fields after comm: state ppid pgrp session tty tpgid flags minflt cminflt majflt cmajflt
    // utime stime cutime cstime prio nice threads itrealvalue starttime vsize rss
    fn stat(pid: u32, comm: &str, ppid: u32, utime: u64, threads: u64, start: u64, rss: u64) -> String {
        format!("{pid} ({comm}) S {ppid} 1 1 0 -1 0 0 0 0 0 {utime} 0 0 0 20 0 {threads} 0 {start} 100000 {rss} 0 0\n")
    }

    fn uid_of(f: &Fixture) -> u32 {
        std::fs::metadata(f.path()).unwrap().uid()
    }

    fn scanner(f: &Fixture) -> ProcessScanner {
        ProcessScanner::new(f.sys(), true, 200).unwrap()
    }

    fn row(t: &Table, pid: u32) -> &Process {
        t.rows.iter().find(|r| r.pid == pid).unwrap()
    }

    fn pause() {
        std::thread::sleep(Duration::from_millis(100));
    }

    #[test]
    fn comm_with_spaces_and_parentheses_is_kept_whole() {
        // splitting on the first ")" would truncate the name and shift every later field
        let f = Fixture::new().file("proc/5/stat", &stat(5, "Web (Content) x", 1, 0, 1, 10, 7));
        let mut s = scanner(&f);
        s.scan().unwrap();
        let t = s.scan().unwrap().unwrap();
        assert_eq!(row(&t, 5).comm, "Web (Content) x");
        assert_eq!(row(&t, 5).ppid, 1);
        assert_eq!(row(&t, 5).rss_bytes, 7 * s.page_size);
    }

    #[test]
    fn first_scan_has_no_rates() {
        let f = Fixture::new().file("proc/5/stat", &stat(5, "a", 1, 0, 1, 10, 7));
        assert!(scanner(&f).scan().unwrap().is_none());
    }

    #[test]
    fn cpu_pct_is_percent_of_one_core() {
        let f = Fixture::new().file("proc/5/stat", &stat(5, "busy", 1, 100, 1, 10, 7));
        let mut s = scanner(&f);
        s.scan().unwrap();
        pause();
        // 5 ticks at 100 Hz is 50 ms of CPU, over at least 100 ms of wall time
        std::fs::write(f.path().join("proc/5/stat"), stat(5, "busy", 1, 105, 1, 10, 7)).unwrap();
        let t = s.scan().unwrap().unwrap();
        let pct = row(&t, 5).cpu_pct;
        assert!((20.0..=50.0).contains(&pct), "{pct}");
    }

    #[test]
    fn reused_pid_is_a_new_process_not_a_huge_cpu_spike() {
        // pid 5 restarts with a different start time and a smaller counter; keyed by pid alone this reads as a delta
        let f = Fixture::new().file("proc/5/stat", &stat(5, "old", 1, 1_000_000, 1, 10, 7));
        let mut s = scanner(&f);
        s.scan().unwrap();
        std::fs::write(f.path().join("proc/5/stat"), stat(5, "new", 1, 500, 1, 99, 7)).unwrap();
        let t = s.scan().unwrap().unwrap();
        assert_eq!(t.count, 1);
        assert!(t.rows.is_empty(), "a process seen once has no rate");
    }

    #[test]
    fn process_that_exits_mid_scan_is_skipped_silently() {
        // pid 6's directory exists but its stat is gone, as when it exits between listing and reading
        let f = Fixture::new()
            .file("proc/5/stat", &stat(5, "a", 1, 0, 1, 10, 7))
            .file("proc/6/cmdline", "gone");
        let mut s = scanner(&f);
        s.scan().unwrap();
        let t = s.scan().unwrap().unwrap();
        assert_eq!(t.count, 1);
        assert_eq!(t.rows.len(), 1);
    }

    #[test]
    fn malformed_stat_is_an_error_not_a_guess() {
        let f = Fixture::new().file("proc/5/stat", "5 (a) S 1\n");
        assert!(scanner(&f).scan().is_err());
    }

    #[test]
    fn io_and_fds_only_where_readable_and_users_from_passwd() {
        let f = Fixture::new();
        let uid = uid_of(&f);
        let f = f
            .file(
                "etc/passwd",
                &format!("root:x:0:0::/root:/bin/sh\nme:x:{uid}:{uid}::/home/me:/bin/sh\n"),
            )
            .file("proc/5/stat", &stat(5, "mine", 1, 0, 1, 10, 7))
            .file("proc/5/io", "rchar: 1\nread_bytes: 1000\nwrite_bytes: 4000\n")
            .file("proc/5/fd/0", "")
            .file("proc/5/fd/1", "")
            .file("proc/5/cmdline", "mine\0--flag\0")
            .file("proc/6/stat", &stat(6, "other", 1, 0, 1, 11, 7));
        let mut s = scanner(&f);
        s.scan().unwrap();
        pause();
        std::fs::write(f.path().join("proc/5/io"), "read_bytes: 2000\nwrite_bytes: 4000\n").unwrap();
        let t = s.scan().unwrap().unwrap();
        let mine = row(&t, 5);
        assert!(mine.read_bps.unwrap() > 0);
        assert_eq!(mine.write_bps, Some(0));
        assert_eq!(mine.fds, Some(2));
        assert_eq!(mine.user.as_deref(), Some("me"));
        assert_eq!(mine.cmdline, "mine --flag");
        // no io file: absent, not zero, so a process that is merely uninspectable never looks idle
        let other = row(&t, 6);
        assert_eq!((other.read_bps, other.write_bps, other.fds), (None, None, None));
    }

    #[test]
    fn unknown_uid_falls_back_to_the_number() {
        let f = Fixture::new().file("proc/5/stat", &stat(5, "a", 1, 0, 1, 10, 7));
        let uid = uid_of(&f);
        let mut s = scanner(&f);
        s.scan().unwrap();
        let t = s.scan().unwrap().unwrap();
        assert_eq!(row(&t, 5).user, Some(uid.to_string()));
    }

    #[test]
    fn cmdline_is_truncated_to_the_option() {
        let f = Fixture::new()
            .file("proc/5/stat", &stat(5, "a", 1, 0, 1, 10, 7))
            .file("proc/5/cmdline", "abcdefghij\0");
        let mut s = ProcessScanner::new(f.sys(), false, 4).unwrap();
        s.scan().unwrap();
        assert_eq!(row(&s.scan().unwrap().unwrap(), 5).cmdline, "abcd");
    }

    #[test]
    fn two_scanners_keep_independent_rate_state() {
        // the on-demand table must not consume the baseline the periodic collector measures against
        let f = Fixture::new().file("proc/5/stat", &stat(5, "a", 1, 0, 1, 10, 7));
        let mut a = scanner(&f);
        let mut b = scanner(&f);
        a.scan().unwrap();
        pause();
        std::fs::write(f.path().join("proc/5/stat"), stat(5, "a", 1, 5, 1, 10, 7)).unwrap();
        assert!(b.scan().unwrap().is_none(), "b has not been primed by a's scan");
        let ta = a.scan().unwrap().unwrap();
        assert!(row(&ta, 5).cpu_pct > 0.0);
        pause();
        let tb = b.scan().unwrap().unwrap();
        assert_eq!(row(&tb, 5).cpu_pct, 0.0, "b saw no change since its own baseline");
    }

    #[test]
    fn ranks_top_lists_and_groups() {
        let f = Fixture::new()
            .file("proc/1/stat", &stat(1, "worker", 0, 0, 2, 10, 100))
            .file("proc/2/stat", &stat(2, "worker", 0, 0, 3, 11, 300))
            .file("proc/3/stat", &stat(3, "solo", 0, 0, 1, 12, 200))
            .file("proc/4/stat", &stat(4, "idle", 0, 0, 1, 13, 0));
        let mut c = Processes::new(
            Options {
                top_n: 2,
                ..Options::default()
            },
            f.sys(),
        )
        .unwrap();
        assert!(c.sample().unwrap().is_none());
        pause();
        for (pid, comm, ticks, start, rss) in [
            (1, "worker", 6, 10, 100),
            (2, "worker", 2, 11, 300),
            (3, "solo", 5, 12, 200),
        ] {
            std::fs::write(
                f.path().join(format!("proc/{pid}/stat")),
                stat(pid, comm, 0, ticks, 1, start, rss),
            )
            .unwrap();
        }
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::processes::Processes>(&v);
        assert_eq!((v["count"].clone(), v["states"]["S"].clone()), (4.into(), 4.into()));
        assert_eq!(v["threads"], 1 + 1 + 1 + 1);
        let pids = |k: &str| {
            v[k].as_array()
                .unwrap()
                .iter()
                .map(|r| r["pid"].as_u64().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(pids("top_cpu"), [1, 3], "ranked by cpu, capped at top_n");
        assert_eq!(pids("top_memory"), [2, 3]);
        // idle processes are not ranked: a zero is not "top"
        assert!(!pids("top_cpu").contains(&4));
        let names = |k: &str| {
            v[k].as_array()
                .unwrap()
                .iter()
                .map(|g| g["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("groups_cpu"), ["worker", "solo"]);
        assert_eq!(v["groups_memory"][0]["name"], "worker");
        assert_eq!(v["groups_memory"][0]["count"], 2);
        assert_eq!(v["groups_memory"][0]["rss_bytes"], 400 * c.scanner.page_size);
    }

    #[test]
    fn options_switch_off_io_and_groups() {
        let f = Fixture::new().file("proc/1/stat", &stat(1, "a", 0, 0, 1, 10, 1));
        let mut c = Processes::new(
            Options {
                io: false,
                groups: false,
                ..Options::default()
            },
            f.sys(),
        )
        .unwrap();
        c.sample().unwrap();
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::processes::Processes>(&v);
        for k in ["top_io", "groups_cpu", "groups_memory"] {
            assert!(v.get(k).is_none(), "{k}");
        }
    }

    #[test]
    fn unknown_option_is_rejected() {
        let t: toml::Table = "top = 3".parse().unwrap();
        assert!(crate::collector::options::<Options>(t).is_err());
    }
}
