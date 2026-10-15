use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::PathBuf;
use std::time::Instant;

use crate::sys::Sys;

/// One DRM client as printed in `/proc/<pid>/fdinfo/<fd>`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Client {
    pub driver: String,
    pub pdev: Option<String>,
    pub id: Option<u64>,
    /// Engine class → busy ns (amdgpu, i915) or busy cycles (xe).
    pub busy: BTreeMap<String, u64>,
    /// xe only: engine class → elapsed GPU timestamp cycles.
    pub total: BTreeMap<String, u64>,
    pub capacity: BTreeMap<String, u64>,
    pub mem_bytes: Option<u64>,
}

pub fn parse(text: &str) -> Option<Client> {
    let mut c = Client::default();
    let (mut resident, mut legacy) = (None, None);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let first = value.split_whitespace().next().and_then(|v| v.parse::<u64>().ok());
        let class = |prefix: &str| Some((key.strip_prefix(prefix)?.to_string(), first?));
        if key == "drm-driver" {
            c.driver = value.to_string();
        } else if key == "drm-pdev" {
            c.pdev = Some(value.to_string());
        } else if key == "drm-client-id" {
            c.id = value.parse().ok();
        } else if key.starts_with("drm-engine-capacity-") {
            c.capacity.extend(class("drm-engine-capacity-"));
        } else if key.starts_with("drm-engine-") {
            c.busy.extend(class("drm-engine-"));
        } else if key.starts_with("drm-total-cycles-") {
            c.total.extend(class("drm-total-cycles-"));
        } else if key.starts_with("drm-cycles-") {
            c.busy.extend(class("drm-cycles-"));
        } else if key.starts_with("drm-resident-") {
            *resident.get_or_insert(0) += bytes(value).unwrap_or(0);
        } else if key.starts_with("drm-memory-") {
            *legacy.get_or_insert(0) += bytes(value).unwrap_or(0);
        }
    }
    if c.driver.is_empty() {
        return None;
    }
    // amdgpu prints both; drm-memory- is the deprecated alias of drm-resident-
    c.mem_bytes = resident.or(legacy);
    Some(c)
}

fn bytes(value: &str) -> Option<u64> {
    let mut words = value.split_whitespace();
    let n: u64 = words.next()?.parse().ok()?;
    match words.next() {
        None => Some(n),
        Some("KiB") => Some(n << 10),
        Some("MiB") => Some(n << 20),
        Some(_) => None,
    }
}

/// One process's use of one device, summed over its clients.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Usage {
    pub pid: u32,
    pub pci: String,
    pub comm: String,
    /// Engine class → percent busy; `None` until a client has been seen twice.
    pub engines: Option<BTreeMap<String, f64>>,
    pub mem_bytes: Option<u64>,
}

struct Prev {
    busy: BTreeMap<String, u64>,
    total: BTreeMap<String, u64>,
    t: Instant,
}

/// Every cached process is fully rescanned once per this many ticks.
const ROTATION: u64 = 5;

/// What one process had open the last time its fds were listed.
struct Cached {
    start: u64,
    /// `(fd, node)` for each fd linking into `/dev/dri/`, in fd-name order.
    fds: Vec<(String, String)>,
    /// The fd directory could not be listed; retried only in the rotation.
    unreadable: bool,
    /// An fd vanished since the last listing.
    rescan: bool,
    seen: u64,
}

#[derive(Default)]
struct Bufs {
    path: PathBuf,
    rel: String,
    text: String,
}

impl Bufs {
    fn read(&mut self, sys: &Sys, rel: std::fmt::Arguments) -> bool {
        self.rel.clear();
        let _ = self.rel.write_fmt(rel);
        sys.path_into(&mut self.path, &self.rel);
        self.text.clear();
        std::fs::File::open(&self.path)
            .and_then(|mut f| f.read_to_string(&mut self.text))
            .is_ok()
    }

    /// Field 22 of `/proc/<pid>/stat`; the command name may hold spaces and parentheses, so count after the last `)`.
    fn start_time(&mut self, sys: &Sys, pid: u32) -> u64 {
        if !self.read(sys, format_args!("/proc/{pid}/stat")) {
            return 0;
        }
        let rest = self.text.rsplit_once(')').map_or("", |(_, r)| r);
        rest.split_whitespace()
            .nth(19)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    fn list_fds(&mut self, sys: &Sys, pid: u32, start: u64, seen: u64) -> Cached {
        self.rel.clear();
        let _ = write!(self.rel, "/proc/{pid}/fd");
        sys.path_into(&mut self.path, &self.rel);
        let mut cached = Cached {
            start,
            fds: Vec::new(),
            unreadable: false,
            rescan: false,
            seen,
        };
        let Ok(dir) = std::fs::read_dir(&self.path) else {
            cached.unreadable = true;
            return cached;
        };
        let mut names: Vec<String> = dir.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
        names.sort();
        for fd in names {
            sys.path_into(&mut self.path, &format!("/proc/{pid}/fd/{fd}"));
            let Ok(target) = std::fs::read_link(&self.path) else {
                continue;
            };
            if let Some(node) = target.to_str().and_then(|t| t.strip_prefix("/dev/dri/")) {
                cached.fds.push((fd, node.to_string()));
            }
        }
        cached
    }
}

#[derive(Default)]
pub struct Scanner {
    prev: HashMap<(String, String), Prev>,
    cache: HashMap<u32, Cached>,
    tick: u64,
    bufs: Bufs,
    pub scanned_before: bool,
}

impl Scanner {
    /// `nodes` maps a `/dev/dri` node name to the PCI slot of the devices to read.
    pub fn scan(&mut self, sys: &Sys, nodes: &HashMap<String, String>, now: Instant) -> Vec<Usage> {
        self.tick += 1;
        let tick = self.tick;
        let mut pids: Vec<u32> = Vec::new();
        sys.path_into(&mut self.bufs.path, "/proc");
        for e in std::fs::read_dir(&self.bufs.path).into_iter().flatten().flatten() {
            if let Some(pid) = e.file_name().to_str().and_then(|n| n.parse().ok()) {
                pids.push(pid);
            }
        }
        for &pid in &pids {
            let start = self.bufs.start_time(sys, pid);
            match self.cache.get_mut(&pid) {
                Some(c) if c.start == start => {
                    c.seen = tick;
                    if c.rescan || u64::from(pid) % ROTATION == tick % ROTATION {
                        *c = self.bufs.list_fds(sys, pid, start, tick);
                    }
                }
                _ => {
                    let c = self.bufs.list_fds(sys, pid, start, tick);
                    self.cache.insert(pid, c);
                }
            }
        }
        self.cache.retain(|_, c| c.seen == tick);

        // visit processes in the order of their names so de-duplication keeps the same client as a directory walk would
        let mut hits: Vec<u32> = self
            .cache
            .iter()
            .filter(|(_, c)| !c.fds.is_empty())
            .map(|(p, _)| *p)
            .collect();
        hits.sort_by_cached_key(|p| p.to_string());

        let mut seen = HashSet::new();
        let mut next = HashMap::new();
        let mut usage: BTreeMap<(u32, String), Usage> = BTreeMap::new();
        for pid_n in hits {
            let pid = pid_n;
            let mut comm = None;
            let mut gone = Vec::new();
            let fds = self.cache[&pid].fds.clone();
            for (fd, node) in &fds {
                if !self.bufs.read(sys, format_args!("/proc/{pid}/fdinfo/{fd}")) {
                    gone.push(fd.clone());
                    continue;
                }
                let Some(pci) = nodes.get(node) else {
                    continue;
                };
                let Some(client) = parse(&self.bufs.text) else {
                    continue;
                };
                let pci = client.pdev.clone().unwrap_or_else(|| pci.clone());
                let id = client.id.map_or_else(|| format!("{pid}/{fd}"), |id| id.to_string());
                let key = (pci.clone(), id);
                // one client reached through dup'ed or passed fds is counted once
                if !seen.insert(key.clone()) {
                    continue;
                }
                if comm.is_none() {
                    comm = sys.read_opt(format!("/proc/{pid}/comm"));
                }
                let Some(comm) = &comm else {
                    continue;
                };
                let (rates, prev) = rates(self.prev.get(&key), &client, now);
                next.insert(key, prev);
                let u = usage.entry((pid_n, pci.clone())).or_insert_with(|| Usage {
                    pid: pid_n,
                    pci,
                    comm: comm.clone(),
                    ..Usage::default()
                });
                if let Some(m) = client.mem_bytes {
                    *u.mem_bytes.get_or_insert(0) += m;
                }
                if let Some(rates) = rates {
                    let engines = u.engines.get_or_insert_with(BTreeMap::new);
                    for (class, pct) in rates {
                        *engines.entry(class).or_insert(0.0) += pct;
                    }
                }
            }
            if !gone.is_empty() {
                let c = self.cache.get_mut(&pid).unwrap();
                c.fds.retain(|(fd, _)| !gone.contains(fd));
                c.rescan = true;
            }
        }
        self.prev = next;
        self.scanned_before = true;
        usage.into_values().collect()
    }
}

/// Percent busy per engine class since the client's previous reading, and the values to keep.
fn rates(prev: Option<&Prev>, c: &Client, now: Instant) -> (Option<BTreeMap<String, f64>>, Prev) {
    // the kernel may briefly report a smaller value; hold the larger one until it catches up
    let held = |old: Option<&BTreeMap<String, u64>>, new: &BTreeMap<String, u64>| -> BTreeMap<String, u64> {
        new.iter()
            .map(|(k, v)| (k.clone(), (*v).max(old.and_then(|o| o.get(k)).copied().unwrap_or(0))))
            .collect()
    };
    let keep = Prev {
        busy: held(prev.map(|p| &p.busy), &c.busy),
        total: held(prev.map(|p| &p.total), &c.total),
        t: now,
    };
    let Some(prev) = prev else {
        return (None, keep);
    };
    let wall_ns = now.duration_since(prev.t).as_nanos() as f64;
    let mut out = BTreeMap::new();
    for (class, busy) in &keep.busy {
        let d_busy = busy.saturating_sub(prev.busy.get(class).copied().unwrap_or(*busy)) as f64;
        let span = match keep.total.get(class) {
            Some(total) => total.saturating_sub(prev.total.get(class).copied().unwrap_or(*total)) as f64,
            None => wall_ns,
        };
        if span <= 0.0 {
            continue;
        }
        let capacity = c.capacity.get(class).copied().unwrap_or(1).max(1) as f64;
        out.insert(class.clone(), (100.0 * d_busy / span / capacity).min(100.0));
    }
    (Some(out), keep)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    use super::*;
    use crate::testutil::Fixture;

    // trimmed from this machine (Hyprland on the Raphael iGPU); note the space before the tab
    const AMDGPU: &str = "pos:\t0\nflags:\t02100002\nmnt_id:\t28\nino:\t1342\ndrm-driver:\tamdgpu\ndrm-client-id:\t68\ndrm-pdev:\t0000:7a:00.0\npasid:\t31\ndrm-total-gtt:\t2 MiB\ndrm-resident-cpu:\t0\ndrm-resident-gtt:\t2 MiB\ndrm-resident-vram:\t12 KiB\ndrm-memory-vram:\t12 KiB\ndrm-memory-gtt: \t2048 KiB\ndrm-memory-cpu: \t0 KiB\namd-requested-vram:\t12 KiB\n";

    #[test]
    fn amdgpu_memory_is_resident_regions_not_resident_plus_legacy_alias() {
        let c = parse(AMDGPU).unwrap();
        assert_eq!(c.driver, "amdgpu");
        assert_eq!((c.pdev.as_deref(), c.id), (Some("0000:7a:00.0"), Some(68)));
        assert_eq!(c.mem_bytes, Some(2 * 1024 * 1024 + 12 * 1024));
        // amdgpu leaves out engines it never used; that is no engine lines, not an error
        assert!(c.busy.is_empty());
    }

    #[test]
    fn legacy_memory_keys_count_when_resident_is_absent() {
        let c = parse("drm-driver:\tamdgpu\ndrm-memory-vram:\t4 KiB\ndrm-memory-gtt: \t1 MiB\n").unwrap();
        assert_eq!(c.mem_bytes, Some(4096 + (1 << 20)));
    }

    #[test]
    fn a_non_drm_fd_is_not_a_client() {
        // the proprietary nvidia driver's nodes carry no drm- keys
        assert_eq!(parse("pos:\t0\nflags:\t02100002\nmnt_id:\t28\nino:\t302\n"), None);
    }

    fn client(busy: &[(&str, u64)], total: &[(&str, u64)], cap: &[(&str, u64)]) -> Client {
        let m = |p: &[(&str, u64)]| p.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        Client {
            driver: "x".into(),
            busy: m(busy),
            total: m(total),
            capacity: m(cap),
            ..Client::default()
        }
    }

    #[test]
    fn engine_ns_over_wall_time_divided_by_capacity() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let (first, p) = rates(None, &client(&[("render", 0), ("video", 0)], &[], &[("video", 2)]), t0);
        assert_eq!(first, None, "one reading is a lifetime total, not a rate");
        let c = client(
            &[("render", 250_000_000), ("video", 1_000_000_000)],
            &[],
            &[("video", 2)],
        );
        let (r, _) = rates(Some(&p), &c, t1);
        let r = r.unwrap();
        assert!((r["render"] - 25.0).abs() < 1e-9);
        // 1 s of busy time spread over two video engines is half their capacity
        assert!((r["video"] - 50.0).abs() < 1e-9, "{r:?}");
    }

    #[test]
    fn xe_cycles_over_gpu_timestamp_cycles() {
        let t0 = Instant::now();
        let (_, p) = rates(None, &client(&[("rcs", 1000)], &[("rcs", 10_000)], &[]), t0);
        // CPU wall time is irrelevant: 3 s passed but the GPU clock advanced 4000 cycles
        let c = client(&[("rcs", 2000)], &[("rcs", 14_000)], &[]);
        let (r, _) = rates(Some(&p), &c, t0 + Duration::from_secs(3));
        assert!((r.unwrap()["rcs"] - 25.0).abs() < 1e-9);
    }

    #[test]
    fn a_brief_counter_decrease_holds_the_larger_value() {
        let t0 = Instant::now();
        let s = Duration::from_secs(1);
        let (_, p) = rates(None, &client(&[("gfx", 500_000_000)], &[], &[]), t0);
        let (r, p) = rates(Some(&p), &client(&[("gfx", 400_000_000)], &[], &[]), t0 + s);
        assert_eq!(r.unwrap()["gfx"], 0.0);
        // measured from the held 500 ms, not from the dip, or the catch-up would double count
        let (r, _) = rates(Some(&p), &client(&[("gfx", 600_000_000)], &[], &[]), t0 + 2 * s);
        assert!((r.unwrap()["gfx"] - 10.0).abs() < 1e-9);
    }

    fn fd(f: &Fixture, pid: u32, fd: u32, node: &str, info: &str) {
        let dir = f.path().join(format!("proc/{pid}"));
        std::fs::create_dir_all(dir.join("fd")).unwrap();
        std::fs::create_dir_all(dir.join("fdinfo")).unwrap();
        symlink(format!("/dev/dri/{node}"), dir.join(format!("fd/{fd}"))).unwrap();
        std::fs::write(dir.join(format!("fdinfo/{fd}")), info).unwrap();
        std::fs::write(dir.join("comm"), format!("p{pid}\n")).unwrap();
    }

    #[test]
    fn scan_dedups_clients_and_skips_other_devices() {
        let f = Fixture::new();
        let a = "drm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\ndrm-client-id:\t7\ndrm-engine-render:\t0 ns\ndrm-resident-system:\t8 MiB\n";
        fd(&f, 10, 3, "renderD128", a);
        // the same client through a dup'ed fd must not double its memory or busy time
        fd(&f, 10, 4, "renderD128", a);
        fd(&f, 10, 5, "renderD129", "drm-driver:\tnvidia\n");
        fd(
            &f,
            11,
            3,
            "card0",
            "drm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\ndrm-client-id:\t9\ndrm-resident-system:\t1 MiB\n",
        );
        // a process that vanished between listing and reading is skipped silently
        std::fs::create_dir_all(f.path().join("proc/12/fd")).unwrap();
        symlink("/dev/dri/card0", f.path().join("proc/12/fd/3")).unwrap();
        let nodes = HashMap::from([
            ("renderD128".to_string(), "0000:00:02.0".to_string()),
            ("card0".to_string(), "0000:00:02.0".to_string()),
        ]);
        let mut s = Scanner::default();
        let t0 = Instant::now();
        let u = s.scan(&f.sys(), &nodes, t0);
        assert_eq!(u.len(), 2);
        assert_eq!(
            (u[0].pid, u[0].comm.as_str(), u[0].mem_bytes),
            (10, "p10", Some(8 << 20))
        );
        assert_eq!(u[0].engines, None);

        let busy = a.replace("render:\t0 ns", "render:\t500000000 ns");
        std::fs::write(f.path().join("proc/10/fdinfo/3"), &busy).unwrap();
        std::fs::write(f.path().join("proc/10/fdinfo/4"), &busy).unwrap();
        let u = s.scan(&f.sys(), &nodes, t0 + Duration::from_secs(1));
        assert!((u[0].engines.as_ref().unwrap()["render"] - 50.0).abs() < 1e-9, "{u:?}");
    }

    fn stat(f: &Fixture, pid: u32, start: u64) {
        let dir = f.path().join(format!("proc/{pid}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("stat"),
            format!("{pid} (a) b) S {}{start} 0\n", "0 ".repeat(18)),
        )
        .unwrap();
    }

    fn client_info(id: u32) -> String {
        format!("drm-driver:\ti915\ndrm-pdev:\t0000:00:02.0\ndrm-client-id:\t{id}\ndrm-resident-system:\t1 MiB\n")
    }

    fn nodes() -> HashMap<String, String> {
        HashMap::from([("renderD128".to_string(), "0000:00:02.0".to_string())])
    }

    fn ticks(s: &mut Scanner, f: &Fixture, t0: Instant, n: u64) -> Vec<Vec<Usage>> {
        (0..n)
            .map(|i| s.scan(&f.sys(), &nodes(), t0 + Duration::from_secs(i)))
            .collect()
    }

    #[test]
    fn a_gpu_opened_after_the_first_scan_is_found_within_the_rotation() {
        let f = Fixture::new();
        stat(&f, 10, 100);
        std::fs::create_dir_all(f.path().join("proc/10/fd")).unwrap();
        let mut s = Scanner::default();
        let t0 = Instant::now();
        assert!(ticks(&mut s, &f, t0, 1)[0].is_empty());
        fd(&f, 10, 3, "renderD128", &client_info(7));
        let seen = ticks(&mut s, &f, t0, ROTATION);
        assert!(seen[0].is_empty(), "served from the cache between rotations");
        assert!(!seen.last().unwrap().is_empty(), "never found: {seen:?}");
    }

    #[test]
    fn a_reused_pid_is_rescanned_at_once() {
        let f = Fixture::new();
        stat(&f, 10, 100);
        fd(&f, 10, 3, "renderD128", &client_info(7));
        let mut s = Scanner::default();
        let t0 = Instant::now();
        assert_eq!(ticks(&mut s, &f, t0, 1)[0].len(), 1);
        std::fs::remove_file(f.path().join("proc/10/fd/3")).unwrap();
        std::fs::remove_file(f.path().join("proc/10/fdinfo/3")).unwrap();
        stat(&f, 10, 200);
        fd(&f, 10, 5, "renderD128", &client_info(8));
        ticks(&mut s, &f, t0, 1);
        assert_eq!(s.cache[&10].start, 200);
        assert_eq!(s.cache[&10].fds, vec![("5".to_string(), "renderD128".to_string())]);
    }

    #[test]
    fn a_closed_fd_is_dropped_and_its_replacement_found_next_tick() {
        let f = Fixture::new();
        stat(&f, 11, 100);
        fd(&f, 11, 3, "renderD128", &client_info(7));
        let mut s = Scanner::default();
        let t0 = Instant::now();
        assert_eq!(ticks(&mut s, &f, t0, 1)[0].len(), 1);
        std::fs::remove_file(f.path().join("proc/11/fd/3")).unwrap();
        std::fs::remove_file(f.path().join("proc/11/fdinfo/3")).unwrap();
        fd(&f, 11, 4, "renderD128", &client_info(7));
        assert!(s.scan(&f.sys(), &nodes(), t0 + Duration::from_secs(1)).is_empty());
        assert!(s.cache[&11].fds.is_empty() && s.cache[&11].rescan);
        let u = s.scan(&f.sys(), &nodes(), t0 + Duration::from_secs(2));
        assert_eq!(u.len(), 1);
        assert_eq!(s.cache[&11].fds, vec![("4".to_string(), "renderD128".to_string())]);
    }

    #[test]
    fn an_unreadable_process_is_not_listed_again_outside_the_rotation() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new();
        stat(&f, 10, 100);
        fd(&f, 10, 3, "renderD128", &client_info(7));
        let fd_dir = f.path().join("proc/10/fd");
        std::fs::set_permissions(&fd_dir, std::fs::Permissions::from_mode(0o0)).unwrap();
        let mut s = Scanner::default();
        let t0 = Instant::now();
        assert!(ticks(&mut s, &f, t0, 1)[0].is_empty());
        if !s.cache[&10].unreadable {
            return; // root can read any directory
        }
        std::fs::set_permissions(&fd_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let seen = ticks(&mut s, &f, t0, ROTATION);
        assert!(seen[0].is_empty(), "retried as if it were new");
        assert!(!seen.last().unwrap().is_empty(), "never retried: {seen:?}");
    }
}
