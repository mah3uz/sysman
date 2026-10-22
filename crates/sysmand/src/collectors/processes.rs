use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::MetadataExt;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use schema::helper::ProcCounters;
use schema::processes::{Group, Process, Table};
use serde_json::{Value, json};

use crate::collector::Collector;
use crate::deltas::Deltas;
use crate::sys::Sys;

pub use sysman_config::modules::processes::Options;

struct Snapshot {
    pid: u32,
    ppid: u32,
    comm: String,
    state: String,
    threads: u64,
    rss_bytes: u64,
    start_time: u64,
    nice: i64,
    priority: i64,
    vsize_bytes: u64,
    cpu_time_s: f64,
}

/// What cannot change once a process instance has started: its user, command line and executable.
#[derive(Clone)]
struct Identity {
    user: Option<String>,
    cmdline: String,
    exe: Option<String>,
}

/// Owns its rate state, so scanners used by different callers do not disturb each other.
pub struct ProcessScanner {
    sys: Sys,
    io: bool,
    cmdline_max: usize,
    clk_tck: f64,
    page_size: u64,
    deltas: Deltas<String>,
    /// The privileged helper's I/O and network counters, keyed by pid and start time.
    helper_io: Deltas<(u32, u64)>,
    helper_net: Deltas<(u32, u64)>,
    users: HashMap<u32, String>,
    identity: HashMap<(u32, u64), Identity>,
    passwd_mtime: Option<SystemTime>,
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
            helper_io: Deltas::default(),
            helper_net: Deltas::default(),
            users: HashMap::new(),
            identity: HashMap::new(),
            passwd_mtime: None,
        })
    }

    /// `None` on the first call, which only primes the counters. Rows carry no `user`, `fds`, `cmdline` or `exe`;
    /// `fill_table` adds them.
    pub fn scan(&mut self) -> Result<Option<Table>> {
        let mut procs: HashMap<String, Snapshot> = HashMap::new();
        let mut counters: HashMap<String, Vec<u64>> = HashMap::new();
        let btime = self.boot_time();
        for pid in self.sys.list("/proc") {
            if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            // the process may exit between the listing and any of these reads
            let Some(stat) = self.sys.read_opt(format!("/proc/{pid}/stat")) else {
                continue;
            };
            let Ok((mut snap, start, ticks)) = self.parse_stat(&pid, &stat) else {
                continue;
            };
            let mut counts = vec![ticks];
            if self.io
                && let Some(io) = self.sys.read_opt(format!("/proc/{pid}/io"))
            {
                counts.extend(parse_io(&io));
            }
            snap.start_time = btime + start / self.clk_tck as u64;
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
                user: None,
                fds: None,
                net_rx_bps: None,
                net_tx_bps: None,
                cmdline: String::new(),
                start_time: p.start_time,
                nice: Some(p.nice),
                priority: Some(p.priority),
                vsize_bytes: Some(p.vsize_bytes),
                cpu_time_s: Some(p.cpu_time_s),
                exe: None,
                cgroup: None,
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

    fn parse_stat(&self, pid: &str, stat: &str) -> Result<(Snapshot, u64, u64)> {
        // comm may contain spaces and parentheses, so split on the last ")"
        let (head, tail) = stat
            .rsplit_once(')')
            .with_context(|| format!("/proc/{pid}/stat: no closing parenthesis"))?;
        let comm = head.split_once('(').map_or("", |(_, c)| c).to_string();
        let f: Vec<&str> = tail.split_whitespace().collect();
        if f.len() < 22 {
            bail!("/proc/{pid}/stat: {} fields after comm, expected at least 22", f.len());
        }
        // f[0] is field 3 of proc(5), so field n is f[n - 3]
        let num = |i: usize| -> Result<u64> {
            f[i].parse()
                .with_context(|| format!("/proc/{pid}/stat: field {:?}", f[i]))
        };
        let signed = |i: usize| -> Result<i64> {
            f[i].parse()
                .with_context(|| format!("/proc/{pid}/stat: field {:?}", f[i]))
        };
        let ticks = num(11)? + num(12)?;
        let snap = Snapshot {
            pid: pid.parse().with_context(|| format!("pid {pid:?}"))?,
            ppid: num(1)? as u32,
            comm,
            state: f[0].to_string(),
            threads: num(17)?,
            rss_bytes: num(21)? * self.page_size,
            start_time: 0,
            nice: signed(16)?,
            priority: signed(15)?,
            vsize_bytes: num(20)?,
            cpu_time_s: ticks as f64 / self.clk_tck,
        };
        Ok((snap, num(19)?, ticks))
    }

    fn boot_time(&self) -> u64 {
        self.sys
            .read_opt("/proc/stat")
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok()))
            .unwrap_or(0)
    }

    fn read_identity(&self, pid: u32) -> Identity {
        let uid = std::fs::metadata(self.sys.path(format!("/proc/{pid}")))
            .ok()
            .map(|m| m.uid());
        let user = uid.map(|u| self.users.get(&u).cloned().unwrap_or_else(|| u.to_string()));
        let cmdline = self
            .sys
            .read_opt(format!("/proc/{pid}/cmdline"))
            .unwrap_or_default()
            .replace('\0', " ");
        let exe = std::fs::read_link(self.sys.path(format!("/proc/{pid}/exe")))
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        Identity {
            user,
            cmdline: cmdline.trim().chars().take(self.cmdline_max).collect(),
            exe,
        }
    }

    #[cfg(test)]
    pub fn cached_identities(&self) -> usize {
        self.identity.len()
    }

    fn apply(r: &mut Process, id: Identity) {
        (r.user, r.cmdline, r.exe) = (id.user, id.cmdline, id.exe);
    }

    fn read_fds(&self, pid: u32) -> Option<u64> {
        std::fs::read_dir(self.sys.path(format!("/proc/{pid}/fd")))
            .ok()
            .map(|d| d.count() as u64)
    }

    /// Fills `user`, `fds`, `cmdline` and `exe`. The last three cost a metadata call, a file read and a link read,
    /// so they are read only for processes not seen on the previous call; `fds` is read every time.
    /// Returns the indices of those first-seen rows.
    pub fn fill_table(&mut self, rows: &mut [Process]) -> Vec<usize> {
        self.refresh_users();
        let mut old = std::mem::take(&mut self.identity);
        let mut new = Vec::new();
        for (i, r) in rows.iter_mut().enumerate() {
            let key = (r.pid, r.start_time);
            let identity = old.remove(&key).unwrap_or_else(|| {
                new.push(i);
                self.read_identity(r.pid)
            });
            r.fds = self.read_fds(r.pid);
            Self::apply(r, identity.clone());
            self.identity.insert(key, identity);
        }
        new
    }

    fn read_cgroup(&self, pid: u32) -> Option<String> {
        let text = self.sys.read_opt(format!("/proc/{pid}/cgroup"))?;
        schema::cgroups::cgroup_path(&text).map(str::to_owned)
    }

    /// Adds what only the privileged helper can read: I/O and open files of processes this user may not
    /// inspect, and network traffic of every process. Rows the scan could read keep their own values.
    pub fn fill_helper(&mut self, rows: &mut [Process]) {
        let Some((t, reading)) = self.sys.helper().read() else {
            return;
        };
        let btime = self.boot_time();
        let key = |c: &ProcCounters| (c.pid, btime + c.start_ticks / self.clk_tck as u64);
        let (mut io, mut net, mut fds) = (HashMap::new(), HashMap::new(), HashMap::new());
        for c in &reading.processes {
            if let (Some(r), Some(w)) = (c.read_bytes, c.write_bytes) {
                io.insert(key(c), vec![r, w]);
            }
            if let (Some(rx), Some(tx)) = (c.net_rx_bytes, c.net_tx_bytes) {
                net.insert(key(c), vec![rx, tx]);
            }
            if let Some(n) = c.fds {
                fds.insert(key(c), n);
            }
        }
        let io = per_second(self.helper_io.update_at(io, t));
        let net = per_second(self.helper_net.update_at(net, t));
        for r in rows {
            let k = (r.pid, r.start_time);
            if self.io
                && r.read_bps.is_none()
                && let Some(&(read, write)) = io.get(&k)
            {
                (r.read_bps, r.write_bps) = (Some(read), Some(write));
            }
            if r.fds.is_none() {
                r.fds = fds.get(&k).copied();
            }
            if let Some(&(rx, tx)) = net.get(&k) {
                (r.net_rx_bps, r.net_tx_bps) = (Some(rx), Some(tx));
            }
        }
    }

    fn refresh_users(&mut self) {
        let mtime = std::fs::metadata(self.sys.path("/etc/passwd"))
            .and_then(|m| m.modified())
            .ok();
        if mtime.is_some() && mtime == self.passwd_mtime {
            return;
        }
        self.passwd_mtime = mtime;
        self.users.clear();
        for line in self.sys.read_opt("/etc/passwd").unwrap_or_default().lines() {
            let mut f = line.split(':');
            if let (Some(name), Some(_), Some(uid)) = (f.next(), f.next(), f.next())
                && let Ok(uid) = uid.parse()
            {
                self.users.entry(uid).or_insert_with(|| name.to_string());
            }
        }
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

/// Pairs of counter differences as rates.
fn per_second<K: Eq + std::hash::Hash>(deltas: Option<(f64, HashMap<K, Vec<u64>>)>) -> HashMap<K, (u64, u64)> {
    let Some((dt, d)) = deltas else {
        return HashMap::new();
    };
    let rate = |v: u64| (v as f64 / dt).round() as u64;
    d.into_iter().map(|(k, v)| (k, (rate(v[0]), rate(v[1])))).collect()
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
        let Some(mut table) = self.scanner.scan()? else {
            // the helper's counters need a first reading too, or its rates start a sample late
            self.scanner.fill_helper(&mut []);
            return Ok(None);
        };
        let n = self.opts.top_n;
        let new = self.scanner.fill_table(&mut table.rows);
        self.scanner.fill_helper(&mut table.rows);
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
            table: Vec::new(),
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
        // after the top lists are taken, so the path reaches only the table and from it the history
        for i in new {
            table.rows[i].cgroup = self.scanner.read_cgroup(table.rows[i].pid);
        }
        out.table = table.rows;
        Ok(Some(serde_json::to_value(out)?))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::testutil::{Fixture, assert_schema};

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
    fn nice_priority_vsize_and_cpu_time_come_from_the_fields_after_comm() {
        // a comm with spaces and ")" shifts every field if counted from the start of the line
        let line = "5 (a) b (c)) R 1 1 1 0 -1 0 0 0 0 0 250 150 0 0 -51 -5 3 0 10 123456789 7 0 0\n";
        let f = Fixture::new().file("proc/5/stat", line);
        let mut s = scanner(&f);
        s.scan().unwrap();
        let t = s.scan().unwrap().unwrap();
        let p = row(&t, 5);
        assert_eq!(p.comm, "a) b (c)");
        assert_eq!((p.priority, p.nice, p.threads), (Some(-51), Some(-5), 3));
        assert_eq!(p.vsize_bytes, Some(123_456_789));
        assert_eq!(p.cpu_time_s, Some(400. / s.clk_tck), "utime plus stime, in seconds");
    }

    #[test]
    fn exe_is_read_once_per_process_and_absent_when_unreadable() {
        let f = Fixture::new()
            .file("proc/5/stat", &stat(5, "a", 1, 0, 1, 10, 7))
            .file("proc/6/stat", &stat(6, "b", 1, 0, 1, 11, 7));
        std::os::unix::fs::symlink("/usr/bin/first", f.path().join("proc/5/exe")).unwrap();
        let mut s = scanner(&f);
        s.scan().unwrap();
        let mut t = s.scan().unwrap().unwrap();
        s.fill_table(&mut t.rows);
        assert_eq!(row(&t, 5).exe.as_deref(), Some("/usr/bin/first"));
        assert_eq!(
            row(&t, 6).exe,
            None,
            "another user's process: no link to read, not an empty path"
        );
        std::fs::remove_file(f.path().join("proc/5/exe")).unwrap();
        std::os::unix::fs::symlink("/usr/bin/second", f.path().join("proc/5/exe")).unwrap();
        let mut t = s.scan().unwrap().unwrap();
        s.fill_table(&mut t.rows);
        assert_eq!(
            row(&t, 5).exe.as_deref(),
            Some("/usr/bin/first"),
            "cached with the command line"
        );
    }

    #[test]
    fn start_time_is_boot_time_plus_start_ticks_in_seconds() {
        // field 22 counts ticks since boot, so it is meaningless as an identity until anchored to btime
        let f = Fixture::new()
            .file("proc/stat", "cpu  1 2 3\nbtime 1700000000\n")
            .file("proc/5/stat", &stat(5, "a", 1, 0, 1, 25_000, 7));
        let mut s = scanner(&f);
        s.scan().unwrap();
        let t = s.scan().unwrap().unwrap();
        assert_eq!(row(&t, 5).start_time, 1_700_000_000 + 25_000 / s.clk_tck as u64);
    }

    #[test]
    fn table_reads_identity_once_per_process_but_fds_every_tick() {
        let f = Fixture::new()
            .file("proc/5/stat", &stat(5, "a", 1, 0, 1, 10, 7))
            .file("proc/5/cmdline", "first\0")
            .file("proc/5/fd/0", "");
        let mut s = scanner(&f);
        s.scan().unwrap();
        let mut t = s.scan().unwrap().unwrap();
        s.fill_table(&mut t.rows);
        assert_eq!((row(&t, 5).cmdline.as_str(), row(&t, 5).fds), ("first", Some(1)));
        // a live process instance cannot change its cmdline, so a re-read would only cost; fds do change
        std::fs::write(f.path().join("proc/5/cmdline"), "second\0").unwrap();
        std::fs::write(f.path().join("proc/5/fd/1"), "").unwrap();
        let mut t = s.scan().unwrap().unwrap();
        s.fill_table(&mut t.rows);
        assert_eq!((row(&t, 5).cmdline.as_str(), row(&t, 5).fds), ("first", Some(2)));
        // a new instance on the same pid is read afresh, and the old entry is dropped
        std::fs::write(f.path().join("proc/5/stat"), stat(5, "a", 1, 0, 1, 99, 7)).unwrap();
        let mut t = s.scan().unwrap().unwrap();
        assert!(t.rows.is_empty(), "a new instance has no rate yet");
        s.fill_table(&mut t.rows);
        assert!(s.identity.is_empty());
        let mut t = s.scan().unwrap().unwrap();
        s.fill_table(&mut t.rows);
        assert_eq!(row(&t, 5).cmdline, "second");
    }

    #[test]
    fn collector_sample_carries_the_whole_table() {
        let f = Fixture::new()
            .file("proc/1/stat", &stat(1, "a", 0, 0, 1, 10, 1))
            .file("proc/2/stat", &stat(2, "b", 0, 0, 1, 11, 1));
        let mut c = Processes::new(
            Options {
                top_n: 1,
                ..Options::default()
            },
            f.sys(),
        )
        .unwrap();
        c.sample().unwrap();
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::processes::Processes>(&v);
        assert_eq!(
            v["table"].as_array().unwrap().len(),
            2,
            "idle processes are in the table, not the top lists"
        );
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
    fn malformed_pid_is_skipped_and_the_others_are_still_reported() {
        let f = Fixture::new()
            .file("proc/5/stat", &stat(5, "good", 1, 0, 1, 10, 7))
            .file("proc/6/stat", "6 (bad) S 1\n")
            .file("proc/7/stat", &stat(7, "also-good", 1, 0, 1, 11, 7));
        let mut s = scanner(&f);
        s.scan().unwrap();
        let t = s.scan().unwrap().unwrap();
        assert_eq!(t.count, 2);
        assert_eq!(t.rows.iter().map(|r| r.pid).collect::<Vec<_>>(), [5, 7]);
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
        let mut t = s.scan().unwrap().unwrap();
        s.fill_table(&mut t.rows);
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
    fn scan_alone_leaves_details_empty() {
        let f = Fixture::new()
            .file("proc/5/stat", &stat(5, "a", 1, 0, 1, 10, 7))
            .file("proc/5/cmdline", "a\0--x\0")
            .file("proc/5/fd/0", "");
        let mut s = scanner(&f);
        s.scan().unwrap();
        let t = s.scan().unwrap().unwrap();
        assert_eq!(
            (row(&t, 5).user.as_deref(), row(&t, 5).fds, row(&t, 5).cmdline.as_str()),
            (None, None, "")
        );
    }

    #[test]
    fn passwd_is_reread_only_when_its_mtime_changes() {
        let f = Fixture::new()
            .file("etc/passwd", "alice:x:1000:1000::/h:/bin/sh\n")
            .file("proc/5/stat", &stat(5, "a", 1, 0, 1, 10, 7));
        let mut s = scanner(&f);
        s.refresh_users();
        assert_eq!(s.users.get(&1000).map(String::as_str), Some("alice"));
        let passwd = f.path().join("etc/passwd");
        let mtime = std::fs::metadata(&passwd).unwrap().modified().unwrap();
        std::fs::write(&passwd, "bob:x:1000:1000::/h:/bin/sh\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&passwd)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        s.refresh_users();
        assert_eq!(
            s.users.get(&1000).map(String::as_str),
            Some("alice"),
            "unchanged mtime, cached map"
        );
        std::fs::File::options()
            .write(true)
            .open(&passwd)
            .unwrap()
            .set_modified(mtime + Duration::from_secs(5))
            .unwrap();
        s.refresh_users();
        assert_eq!(s.users.get(&1000).map(String::as_str), Some("bob"));
    }

    #[test]
    fn periodic_collector_rows_carry_details() {
        let f = Fixture::new()
            .file("proc/5/stat", &stat(5, "busy", 1, 0, 1, 10, 7))
            .file("proc/5/cmdline", "busy\0--flag\0")
            .file("proc/5/fd/0", "");
        let mut c = Processes::new(Options::default(), f.sys()).unwrap();
        assert!(c.sample().unwrap().is_none());
        pause();
        std::fs::write(f.path().join("proc/5/stat"), stat(5, "busy", 1, 50, 1, 10, 7)).unwrap();
        let v = c.sample().unwrap().unwrap();
        let top = &v["top_cpu"][0];
        assert_eq!(top["pid"], 5);
        assert_eq!(top["cmdline"], "busy --flag");
        assert_eq!(top["fds"], 1);
        assert!(top["user"].is_string());
    }

    #[test]
    fn unknown_uid_falls_back_to_the_number() {
        let f = Fixture::new().file("proc/5/stat", &stat(5, "a", 1, 0, 1, 10, 7));
        let uid = uid_of(&f);
        let mut s = scanner(&f);
        s.scan().unwrap();
        let mut t = s.scan().unwrap().unwrap();
        s.fill_table(&mut t.rows);
        assert_eq!(row(&t, 5).user, Some(uid.to_string()));
    }

    #[test]
    fn cmdline_is_truncated_to_the_option() {
        let f = Fixture::new()
            .file("proc/5/stat", &stat(5, "a", 1, 0, 1, 10, 7))
            .file("proc/5/cmdline", "abcdefghij\0");
        let mut s = ProcessScanner::new(f.sys(), false, 4).unwrap();
        s.scan().unwrap();
        let mut t = s.scan().unwrap().unwrap();
        s.fill_table(&mut t.rows);
        assert_eq!(row(&t, 5).cmdline, "abcd");
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

    /// pid 5 is another user's process the scan cannot read I/O or fds of; pid 6 is the daemon user's own.
    fn helper_host() -> Fixture {
        Fixture::new()
            .file("proc/stat", "cpu  1 2 3\nbtime 1700000000\n")
            .file("proc/5/stat", &stat(5, "other", 1, 0, 1, 10, 7))
            .file("proc/6/stat", &stat(6, "own", 1, 0, 1, 20, 7))
            .file("proc/6/io", "read_bytes: 0\nwrite_bytes: 0\n")
            .file("proc/6/fd/0", "")
    }

    fn rows(v: &Value) -> Vec<Value> {
        v["table"].as_array().unwrap().clone()
    }

    /// Counts opens of a fixture file through inotify, so a test sees every read of it the code under test makes.
    struct Opens(std::os::fd::OwnedFd);

    impl Opens {
        fn of(path: &std::path::Path) -> Self {
            use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
            use std::os::unix::ffi::OsStrExt;
            let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
            // SAFETY: the fd is checked before it is owned, and the path is a valid C string.
            unsafe {
                let fd = libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC);
                assert!(fd >= 0, "inotify_init1");
                let fd = OwnedFd::from_raw_fd(fd);
                // watching closes too keeps successive opens from being merged into one event
                let mask = libc::IN_OPEN | libc::IN_CLOSE_NOWRITE;
                assert!(libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask) >= 0);
                Self(fd)
            }
        }

        /// Opens since the last call.
        fn take(&self) -> usize {
            use std::os::fd::AsRawFd;
            let mut buf = [0u8; 4096];
            let mut opens = 0;
            loop {
                // SAFETY: reads at most the buffer's length into it.
                let len = unsafe { libc::read(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if len <= 0 {
                    return opens;
                }
                let mut at = 0;
                while at < len as usize {
                    // SAFETY: the kernel writes whole events, each a header followed by `len` name bytes.
                    let ev: libc::inotify_event = unsafe { std::ptr::read_unaligned(buf[at..].as_ptr().cast()) };
                    opens += usize::from(ev.mask & libc::IN_OPEN != 0);
                    at += std::mem::size_of::<libc::inotify_event>() + ev.len as usize;
                }
            }
        }
    }

    #[test]
    fn cgroup_is_read_once_per_instance_and_handed_over_only_in_the_table() {
        let f = Fixture::new()
            .file("proc/5/stat", &stat(5, "a", 1, 0, 1, 10, 7))
            .file("proc/5/cgroup", "0::/system.slice/a.service\n");
        let opens = Opens::of(&f.path().join("proc/5/cgroup"));
        let mut c = Processes::new(Options::default(), f.sys()).unwrap();
        assert!(c.sample().unwrap().is_none());
        assert_eq!(
            opens.take(),
            0,
            "a process seen once has no row, so nothing reaches the history yet"
        );
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::processes::Processes>(&v);
        assert_eq!(rows(&v)[0]["cgroup"], "/system.slice/a.service");
        assert_eq!(v["top_memory"][0]["pid"], 5);
        assert!(
            v["top_memory"][0].get("cgroup").is_none(),
            "the top lists reach the crash log and snapshots every tick"
        );
        assert_eq!(opens.take(), 1);
        for _ in 0..3 {
            let v = c.sample().unwrap().unwrap();
            assert!(rows(&v)[0].get("cgroup").is_none(), "the history already has it");
        }
        assert_eq!(opens.take(), 0, "a live instance is never read again");
        // a new instance on the same pid is a new process with its own cgroup
        std::fs::write(f.path().join("proc/5/stat"), stat(5, "a", 1, 0, 1, 99, 7)).unwrap();
        std::fs::write(f.path().join("proc/5/cgroup"), "0::/system.slice/b.service\n").unwrap();
        opens.take();
        let v = c.sample().unwrap().unwrap();
        assert!(v.get("table").is_none(), "a new instance has no rate yet");
        let v = c.sample().unwrap().unwrap();
        assert_eq!(rows(&v)[0]["cgroup"], "/system.slice/b.service");
        assert_eq!(opens.take(), 1);
    }

    #[test]
    fn a_process_whose_cgroup_cannot_be_read_has_none_and_the_sample_still_succeeds() {
        // as when the process exits between the scan and the read
        let f = Fixture::new().file("proc/5/stat", &stat(5, "a", 1, 0, 1, 10, 7));
        let mut c = Processes::new(Options::default(), f.sys()).unwrap();
        c.sample().unwrap();
        let v = c.sample().unwrap().unwrap();
        assert_eq!(rows(&v)[0]["pid"], 5);
        assert!(rows(&v)[0].get("cgroup").is_none());
    }

    #[test]
    fn helper_fills_unreadable_io_and_fds_and_network_for_all() {
        let f = helper_host();
        let n = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let seen = n.clone();
        f.fake_helper(move |_| {
            let k = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst).min(1);
            json!({"processes": [
                {"pid": 5, "start_ticks": 10, "read_bytes": k * 1_000_000, "write_bytes": k * 2_000_000, "fds": 9,
                 "net_rx_bytes": k * 300_000, "net_tx_bytes": k * 400_000},
                {"pid": 6, "start_ticks": 20, "read_bytes": k * 5_000_000, "write_bytes": 0, "fds": 99,
                 "net_rx_bytes": k * 600_000, "net_tx_bytes": 0},
                // the same pid as an earlier instance: its counters belong to someone else
                {"pid": 5, "start_ticks": 99_999, "net_rx_bytes": 1, "net_tx_bytes": 1}
            ]})
        });
        let mut c = Processes::new(Options::default(), f.sys()).unwrap();
        c.sample().unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::processes::Processes>(&v);
        let t = rows(&v);
        let (other, own) = (&t[0], &t[1]);
        // rates are counter deltas over about half a second
        let near = |x: &Value, per_half_second: f64| {
            let x = x.as_f64().unwrap();
            (per_half_second * 1.6..=per_half_second * 2.0).contains(&x)
        };
        assert!(
            near(&other["read_bps"], 1e6) && near(&other["write_bps"], 2e6),
            "{other}"
        );
        assert!(
            near(&other["net_rx_bps"], 3e5) && near(&other["net_tx_bps"], 4e5),
            "{other}"
        );
        assert_eq!(other["fds"], 9);
        // what the scan read itself is kept; the helper only adds what it could not
        assert_eq!((own["read_bps"].as_u64(), own["fds"].as_u64()), (Some(0), Some(1)));
        assert!(near(&own["net_rx_bps"], 6e5), "{own}");
        assert_eq!(v["top_io"][0]["pid"], 5, "helper I/O counts in the top list");
    }

    #[test]
    fn without_the_helper_rows_are_as_before() {
        let f = helper_host();
        let mut c = Processes::new(Options::default(), f.sys()).unwrap();
        c.sample().unwrap();
        let v = c.sample().unwrap().unwrap();
        let t = rows(&v);
        assert_eq!(
            t[0].as_object()
                .unwrap()
                .keys()
                .filter(|k| ["read_bps", "write_bps", "fds", "net_rx_bps", "net_tx_bps"].contains(&k.as_str()))
                .count(),
            0,
            "{}",
            t[0]
        );
        assert!(t[1].get("net_rx_bps").is_none());
        assert_eq!(t[1]["fds"], 1);
    }
}
