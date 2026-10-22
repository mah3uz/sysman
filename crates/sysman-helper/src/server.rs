use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use schema::helper::{Reading, Request, Sockets, Status};
use serde_json::json;

use crate::net::Net;
use crate::{procs, rapl, smart};

const MIN_GAP: Duration = Duration::from_secs(1);
const MAX_CLIENTS: usize = 8;
const MAX_LINE: u64 = 256;
const IDLE: Duration = Duration::from_secs(60);

/// Every data source; each one that fails is reported in `status` and leaves the others working.
pub struct Sources {
    pub root: PathBuf,
    pub net: Result<Net, String>,
    pub smart: Arc<Mutex<smart::Latest>>,
}

impl Sources {
    pub fn status(&self) -> Status {
        let mut features = BTreeMap::new();
        let mut errors = BTreeMap::new();
        let mut set = |name: &str, result: Result<(), String>| {
            features.insert(name.to_string(), result.is_ok());
            if let Err(e) = result {
                errors.insert(name.to_string(), e);
            }
        };
        set("rapl", rapl::read(&self.root).map(drop).map_err(|e| format!("{e:#}")));
        // pid 1 belongs to root, so reading its counters shows whether other users' processes are readable
        let init = self.root.join("proc/1/io");
        set(
            "proc_io",
            std::fs::read(&init)
                .map(drop)
                .map_err(|e| format!("{}: {e}", init.display())),
        );
        let (smart_ok, smart_error) = {
            let smart = self.smart.lock().expect("smart lock");
            (!smart.drives.is_empty(), smart.error.clone())
        };
        let smart_error = smart_error.or_else(|| (!smart_ok).then(|| "first pass not finished".into()));
        set(
            "smart",
            if smart_ok {
                Ok(())
            } else {
                Err(smart_error.clone().unwrap_or_default())
            },
        );
        set("net", self.net.as_ref().map(drop).map_err(Clone::clone));
        // some drives read and others not: the feature works, and the reason for the rest is kept
        if let (true, Some(e)) = (smart_ok, smart_error) {
            errors.insert("smart".into(), e);
        }
        if let Ok(Net { missing: Some(e), .. }) = &self.net {
            errors.insert("net".into(), e.clone());
        }
        Status {
            version: env!("CARGO_PKG_VERSION").to_string(),
            features,
            errors,
        }
    }

    pub fn read(&self) -> Reading {
        let net = self.net.as_ref().ok().map(Net::counts);
        Reading {
            rapl: rapl::read(&self.root).unwrap_or_default(),
            processes: procs::read(&self.root, net.as_ref()),
            smart: self.smart.lock().expect("smart lock").drives.clone(),
        }
    }

    pub fn sockets(&self) -> Sockets {
        Sockets {
            processes: procs::sockets(&self.root),
        }
    }
}

/// Spaces a client's answers at least `MIN_GAP` apart.
#[derive(Default)]
struct RateLimit {
    last: Option<Instant>,
}

impl RateLimit {
    fn wait(&self, now: Instant) -> Duration {
        self.last
            .map_or(Duration::ZERO, |last| (last + MIN_GAP).saturating_duration_since(now))
    }

    fn answered(&mut self, now: Instant) {
        self.last = Some(now);
    }
}

pub fn serve(listener: UnixListener, sources: Arc<Sources>) {
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming().flatten() {
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CLIENTS {
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let (sources, counter) = (sources.clone(), active.clone());
        let spawned = thread::Builder::new().name("client".into()).spawn(move || {
            let _ = client(stream, &sources);
            counter.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn client(stream: UnixStream, sources: &Sources) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IDLE))?;
    let mut out = stream.try_clone()?;
    let mut input = BufReader::new(stream);
    let mut limit = RateLimit::default();
    loop {
        let mut line = String::new();
        if (&mut input).take(MAX_LINE).read_line(&mut line)? == 0 {
            return Ok(());
        }
        if !line.ends_with('\n') {
            writeln!(out, "{}", json!({"error": "request line too long"}))?;
            return Ok(());
        }
        thread::sleep(limit.wait(Instant::now()));
        writeln!(out, "{}", answer(&line, sources))?;
        limit.answered(Instant::now());
    }
}

fn answer(line: &str, sources: &Sources) -> String {
    let reply = match serde_json::from_str::<Request>(line) {
        Ok(Request::Status) => serde_json::to_value(sources.status()),
        Ok(Request::Read) => serde_json::to_value(sources.read()),
        Ok(Request::Sockets) => serde_json::to_value(sources.sockets()),
        Err(_) => Ok(json!({"error": r#"expected {"cmd":"status"}, {"cmd":"read"} or {"cmd":"sockets"}"#})),
    };
    reply.unwrap_or_else(|e| json!({"error": e.to_string()})).to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn sources(root: &std::path::Path) -> Sources {
        Sources {
            root: root.to_path_buf(),
            net: Err("loading: Operation not permitted".into()),
            smart: Arc::default(),
        }
    }

    #[test]
    fn answers_are_spaced_a_second_apart_per_client() {
        // a client polling in a loop must not turn every /proc scan into a busy loop as root
        let t0 = Instant::now();
        let mut l = RateLimit::default();
        assert_eq!(l.wait(t0), Duration::ZERO, "the first answer is immediate");
        l.answered(t0);
        assert_eq!(l.wait(t0 + Duration::from_millis(300)), Duration::from_millis(700));
        assert_eq!(l.wait(t0 + Duration::from_millis(1200)), Duration::ZERO);
    }

    #[test]
    fn failing_sources_are_off_with_a_reason_and_the_rest_still_answer() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("proc/1")).unwrap();
        std::fs::write(root.path().join("proc/1/io"), "read_bytes: 1\nwrite_bytes: 2\n").unwrap();
        let s = sources(root.path());
        let status = s.status();
        assert!(status.features["proc_io"]);
        for off in ["rapl", "smart", "net"] {
            assert!(!status.features[off], "{off}");
            assert!(!status.errors[off].is_empty(), "{off} needs a reason");
        }
        assert!(s.read().rapl.is_empty());
    }

    #[test]
    fn unknown_requests_get_an_error_not_a_disconnect() {
        let root = tempfile::tempdir().unwrap();
        let s = sources(root.path());
        let reply: Value = serde_json::from_str(&answer(r#"{"cmd":"exec","arg":"rm"}"#, &s)).unwrap();
        assert!(reply["error"].as_str().unwrap().contains("status"));
        let reply: Value = serde_json::from_str(&answer("{\"cmd\":\"status\"}\n", &s)).unwrap();
        assert_eq!(reply["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn sockets_answer_carries_pid_start_and_inodes_and_nothing_else() {
        // the socket's group may read this; any extra field widens what the helper reveals as root
        let root = tempfile::tempdir().unwrap();
        let p = root.path().join("proc/9");
        std::fs::create_dir_all(p.join("fd")).unwrap();
        std::fs::write(
            p.join("stat"),
            "9 (x) S 1 9 9 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 555 1 1 0\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("socket:[31337]", p.join("fd/3")).unwrap();
        std::os::unix::fs::symlink("/etc/shadow", p.join("fd/4")).unwrap();
        let reply: Value = serde_json::from_str(&answer("{\"cmd\":\"sockets\"}\n", &sources(root.path()))).unwrap();
        assert_eq!(
            reply,
            serde_json::json!({"processes": [{"pid": 9, "start_ticks": 555, "inodes": [31337]}]})
        );
    }
}
