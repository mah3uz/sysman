use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use schema::helper::{Reading, Request, Sockets, Status};
use serde::de::DeserializeOwned;
use serde_json::Value;

pub const SOCKET: &str = "/run/sysman-helper/helper.sock";
/// Well inside the tick deadline, so a stuck helper costs a tick its helper data and nothing else.
const TIMEOUT: Duration = Duration::from_millis(500);
/// Modules running in the same tick share one reading; ticks are at least a second apart.
const FRESH: Duration = Duration::from_millis(400);
/// `sockets` is asked on demand while the app waits up to 5 s, and walks every fd on the machine.
const SOCKETS_TIMEOUT: Duration = Duration::from_secs(3);
/// Any number of clients asking for listeners within this share one helper walk.
const SOCKETS_FRESH: Duration = Duration::from_secs(1);

/// Talks to the optional privileged `sysman-helper`, one connection per request.
#[derive(Debug)]
pub struct HelperClient {
    path: PathBuf,
    latest: Mutex<Option<(Instant, Option<Arc<Reading>>)>>,
    /// Held across the request, so at most one `sockets` is in flight.
    sockets: Mutex<Option<(Instant, SocketsAnswer)>>,
}

type SocketsAnswer = Result<Arc<Sockets>, String>;

impl HelperClient {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            latest: Mutex::new(None),
            sockets: Mutex::new(None),
        }
    }

    /// False when the helper is not enabled or not installed.
    pub fn present(&self) -> bool {
        self.path.symlink_metadata().is_ok()
    }

    /// The helper's counters and when they were read; `None` while it is off or failing.
    pub fn read(&self) -> Option<(Instant, Arc<Reading>)> {
        if !self.present() {
            return None;
        }
        let mut latest = self.latest.lock().expect("helper lock");
        if let Some((t, reading)) = latest.as_ref()
            && t.elapsed() < FRESH
        {
            return reading.clone().map(|r| (*t, r));
        }
        let reading = self.request::<Reading>(Request::Read, TIMEOUT).ok().map(Arc::new);
        let t = Instant::now();
        *latest = Some((t, reading.clone()));
        reading.map(|r| (t, r))
    }

    pub fn status(&self) -> Result<Status> {
        self.request(Request::Status, TIMEOUT)
    }

    /// The listening sockets of every process, or why they are not known.
    pub fn sockets(&self) -> SocketsAnswer {
        if !self.present() {
            return Err("sysman-helper is not running".into());
        }
        let mut cached = self.sockets.lock().expect("helper sockets lock");
        if let Some((t, answer)) = cached.as_ref()
            && t.elapsed() < SOCKETS_FRESH
        {
            return answer.clone();
        }
        let answer = self
            .request::<Sockets>(Request::Sockets, SOCKETS_TIMEOUT)
            .map(Arc::new)
            .map_err(|e| match e.downcast_ref::<HelperError>() {
                // an older helper lists the requests it knows, and sockets is not one of them
                Some(HelperError(msg)) if msg.starts_with("expected ") && !msg.contains("sockets") => {
                    "sysman-helper is older than sysmand and cannot list sockets; restart it".to_string()
                }
                Some(HelperError(msg)) => format!("sysman-helper: {msg}"),
                None => format!("{e:#}"),
            });
        *cached = Some((Instant::now(), answer.clone()));
        answer
    }

    fn request<T: DeserializeOwned>(&self, request: Request, timeout: Duration) -> Result<T> {
        let deadline = Instant::now() + timeout;
        let mut stream =
            UnixStream::connect(&self.path).with_context(|| format!("connecting to {}", self.path.display()))?;
        stream.set_write_timeout(Some(timeout))?;
        writeln!(stream, "{}", serde_json::to_string(&request)?)?;
        let mut reply = Vec::new();
        let mut chunk = [0u8; 65536];
        while !reply.ends_with(b"\n") {
            let left = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .context("sysman-helper gave no answer in time")?;
            stream.set_read_timeout(Some(left))?;
            let n = stream
                .read(&mut chunk)
                .context("sysman-helper gave no answer in time")?;
            if n == 0 {
                bail!("sysman-helper closed the connection");
            }
            reply.extend_from_slice(&chunk[..n]);
        }
        let value: Value = serde_json::from_slice(&reply).context("the helper's answer is not JSON")?;
        if let Some(e) = value.get("error").and_then(Value::as_str) {
            return Err(HelperError(e.to_string()).into());
        }
        serde_json::from_value(value).context("the helper's answer does not fit the schema")
    }
}

/// An `{"error": …}` answer, as opposed to a failure to get one.
#[derive(Debug)]
struct HelperError(String);

impl std::fmt::Display for HelperError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HelperError {}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testutil::Fixture;

    #[test]
    fn a_silent_helper_costs_at_most_the_timeout() {
        // a tick has 1.5 s for every module; a hung helper must not take them with it
        let f = Fixture::new();
        f.fake_helper(|_| {
            std::thread::sleep(Duration::from_secs(5));
            json!({})
        });
        let t0 = Instant::now();
        assert!(f.sys().helper().read().is_none());
        assert!(
            t0.elapsed() < TIMEOUT + Duration::from_millis(200),
            "{:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn modules_in_one_tick_share_a_single_request() {
        let f = Fixture::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        f.fake_helper(move |_| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            json!({})
        });
        let sys = f.sys();
        let (t1, _) = sys.helper().read().unwrap();
        let (t2, _) = sys.clone().helper().read().unwrap();
        assert_eq!((t1, calls.load(std::sync::atomic::Ordering::SeqCst)), (t2, 1));
    }

    #[test]
    fn listener_requests_at_once_share_one_sockets_walk_until_it_is_a_second_old() {
        // each walk reads every fd on the machine as root; clients asking in a loop must not multiply it
        let f = Fixture::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = calls.clone();
        f.fake_helper(move |_| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(200));
            json!({"processes": [{"pid": 1, "start_ticks": 1, "inodes": [7]}]})
        });
        let sys = f.sys();
        let start = Arc::new(std::sync::Barrier::new(8));
        let askers: Vec<_> = (0..8)
            .map(|_| {
                let (sys, start) = (sys.clone(), start.clone());
                std::thread::spawn(move || {
                    start.wait();
                    sys.helper().sockets()
                })
            })
            .collect();
        for a in askers {
            assert_eq!(a.join().unwrap().unwrap().processes[0].inodes, vec![7]);
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        std::thread::sleep(SOCKETS_FRESH);
        sys.helper().sockets().unwrap();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "a stale answer is asked again"
        );
    }

    #[test]
    fn sockets_waits_longer_than_a_tick_but_not_forever() {
        // listeners is asked on demand while the app waits; a walk of a busy machine takes longer than a tick may
        let slow = Fixture::new();
        slow.fake_helper(|_| {
            std::thread::sleep(TIMEOUT * 2);
            json!({})
        });
        assert!(slow.sys().helper().sockets().is_ok());

        let hung = Fixture::new();
        hung.fake_helper(|_| {
            std::thread::sleep(SOCKETS_TIMEOUT * 2);
            json!({})
        });
        let t0 = Instant::now();
        let error = hung.sys().helper().sockets().unwrap_err();
        assert!(error.contains("no answer in time"), "{error}");
        assert!(
            t0.elapsed() < SOCKETS_TIMEOUT + Duration::from_millis(200),
            "{:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn an_error_answer_is_an_error() {
        let f = Fixture::new();
        f.fake_helper(|_| json!({"error": "no"}));
        assert_eq!(f.sys().helper().status().unwrap_err().to_string(), "no");
    }
}
