use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use schema::helper::{Reading, Request, Status};
use serde::de::DeserializeOwned;
use serde_json::Value;

pub const SOCKET: &str = "/run/sysman-helper/helper.sock";
/// Well inside the tick deadline, so a stuck helper costs a tick its helper data and nothing else.
const TIMEOUT: Duration = Duration::from_millis(500);
/// Modules running in the same tick share one reading; ticks are at least a second apart.
const FRESH: Duration = Duration::from_millis(400);

/// Talks to the optional privileged `sysman-helper`, one connection per request.
#[derive(Debug)]
pub struct HelperClient {
    path: PathBuf,
    latest: Mutex<Option<(Instant, Option<Arc<Reading>>)>>,
}

impl HelperClient {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            latest: Mutex::new(None),
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
        let reading = self.request::<Reading>(Request::Read).ok().map(Arc::new);
        let t = Instant::now();
        *latest = Some((t, reading.clone()));
        reading.map(|r| (t, r))
    }

    pub fn status(&self) -> Result<Status> {
        self.request(Request::Status)
    }

    fn request<T: DeserializeOwned>(&self, request: Request) -> Result<T> {
        let deadline = Instant::now() + TIMEOUT;
        let mut stream =
            UnixStream::connect(&self.path).with_context(|| format!("connecting to {}", self.path.display()))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        writeln!(stream, "{}", serde_json::to_string(&request)?)?;
        let mut reply = Vec::new();
        let mut chunk = [0u8; 65536];
        while !reply.ends_with(b"\n") {
            let left = deadline
                .checked_duration_since(Instant::now())
                .filter(|d| !d.is_zero())
                .context("no answer in time")?;
            stream.set_read_timeout(Some(left))?;
            let n = stream.read(&mut chunk).context("no answer in time")?;
            if n == 0 {
                bail!("the helper closed the connection");
            }
            reply.extend_from_slice(&chunk[..n]);
        }
        let value: Value = serde_json::from_slice(&reply).context("the helper's answer is not JSON")?;
        if let Some(e) = value.get("error").and_then(Value::as_str) {
            bail!("{e}");
        }
        serde_json::from_value(value).context("the helper's answer does not fit the schema")
    }
}

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
    fn an_error_answer_is_an_error() {
        let f = Fixture::new();
        f.fake_helper(|_| json!({"error": "no"}));
        assert_eq!(f.sys().helper().status().unwrap_err().to_string(), "no");
    }
}
