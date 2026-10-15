use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::collectors::processes::ProcessScanner;
use crate::daemon::unix_now;
use crate::sys::Sys;

/// Lines a subscriber may fall behind before it is disconnected.
const SUBSCRIBER_QUEUE: usize = 32;

#[derive(Default)]
struct State {
    meta: Option<Arc<String>>,
    /// The latest sample's envelope plus each module's most recent value.
    latest: Map<String, Value>,
    subscribers: Vec<SyncSender<Arc<String>>>,
}

/// What the daemon has most recently produced, shared with socket clients.
#[derive(Default)]
pub struct Hub {
    state: Mutex<State>,
}

impl Hub {
    pub fn publish(&self, record: &Value) {
        let line = Arc::new(record.to_string());
        let mut state = self.state.lock().expect("hub lock");
        match record["type"].as_str() {
            Some("meta") => state.meta = Some(line.clone()),
            Some("sample") => {
                for (k, v) in record.as_object().into_iter().flatten() {
                    if k != "errors" {
                        state.latest.insert(k.clone(), v.clone());
                    }
                }
            }
            _ => {}
        }
        // a client that cannot keep up is dropped rather than allowed to hold up the daemon
        state.subscribers.retain(|tx| match tx.try_send(line.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        });
    }

    fn subscribe(&self) -> (Option<Arc<String>>, Receiver<Arc<String>>) {
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_QUEUE);
        let mut state = self.state.lock().expect("hub lock");
        state.subscribers.push(tx);
        (state.meta.clone(), rx)
    }
}

/// True when a running daemon answers on `path`.
pub fn in_use(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

/// Binds the socket and serves clients on background threads.
pub fn serve(path: &Path, hub: Arc<Hub>, sys: Sys) -> Result<()> {
    if in_use(path) {
        bail!("{} is in use; is another sysmand running?", path.display());
    }
    // nothing answered, so any file there is left over from a daemon that died
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    thread::Builder::new().name("socket".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let (hub, sys) = (hub.clone(), sys.clone());
            let _ = thread::Builder::new().name("client".into()).spawn(move || {
                let _ = client(stream, &hub, sys);
            });
        }
    })?;
    Ok(())
}

fn client(stream: UnixStream, hub: &Hub, sys: Sys) -> Result<()> {
    let mut out = stream.try_clone()?;
    let mut scanner: Option<ProcessScanner> = None;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        let cmd = serde_json::from_str::<Value>(&line)
            .ok()
            .and_then(|v| v["cmd"].as_str().map(str::to_owned));
        let reply = match cmd.as_deref() {
            Some("meta") => hub
                .state
                .lock()
                .expect("hub lock")
                .meta
                .as_deref()
                .cloned()
                .unwrap_or_else(|| error("no meta record yet")),
            Some("latest") => {
                let state = hub.state.lock().expect("hub lock");
                if state.latest.is_empty() {
                    error("no sample yet")
                } else {
                    Value::Object(state.latest.clone()).to_string()
                }
            }
            Some("processes") => processes(&mut scanner, &sys).unwrap_or_else(|e| error(&format!("{e:#}"))),
            Some("subscribe") => return stream_records(hub, &mut out),
            Some(other) => error(&format!(
                "unknown command {other:?}; known: meta, latest, subscribe, processes"
            )),
            None => error(r#"expected a JSON object like {"cmd":"latest"}"#),
        };
        writeln!(out, "{reply}")?;
    }
    Ok(())
}

fn stream_records(hub: &Hub, out: &mut UnixStream) -> Result<()> {
    let (meta, rx) = hub.subscribe();
    if let Some(meta) = meta {
        writeln!(out, "{meta}")?;
    }
    for line in rx {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

/// The full process table, with CPU rates relative to this client's previous request.
fn processes(scanner: &mut Option<ProcessScanner>, sys: &Sys) -> Result<String> {
    if scanner.is_none() {
        let mut fresh = ProcessScanner::new(sys.clone(), true, 4096)?;
        fresh.scan()?;
        fresh.fill_helper(&mut []);
        // a rate needs two readings; half a second keeps the first answer quick
        thread::sleep(Duration::from_millis(500));
        *scanner = Some(fresh);
    }
    let scanner = scanner.as_mut().expect("set above");
    let mut table = scanner.scan()?.context("no rates yet")?;
    scanner.fill_table(&mut table.rows);
    scanner.fill_helper(&mut table.rows);
    let mut reply = serde_json::to_value(table)?;
    reply["type"] = "processes".into();
    reply["t"] = json!((unix_now() * 1000.0).round() / 1000.0);
    Ok(reply.to_string())
}

fn error(msg: &str) -> String {
    json!({ "error": msg }).to_string()
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;
    use crate::testutil::Fixture;

    fn start(hub: &Arc<Hub>) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        serve(&path, hub.clone(), Sys::host()).unwrap();
        (dir, path)
    }

    fn ask(path: &Path, cmd: &str) -> Value {
        let mut s = UnixStream::connect(path).unwrap();
        writeln!(s, r#"{{"cmd":"{cmd}"}}"#).unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn sample(t: f64, module: &str, v: i64) -> Value {
        json!({"type": "sample", "schema": 3, "t": t, "time": "x", module: v})
    }

    #[test]
    fn latest_keeps_each_modules_most_recent_value() {
        // slow modules appear on few ticks; a client asking between them still needs their last value
        let hub = Arc::new(Hub::default());
        let (_dir, path) = start(&hub);
        hub.publish(&sample(1.0, "fs", 10));
        hub.publish(&sample(2.0, "cpu", 5));
        hub.publish(&sample(3.0, "cpu", 6));
        let latest = ask(&path, "latest");
        assert_eq!(
            (latest["fs"].as_i64(), latest["cpu"].as_i64(), latest["t"].as_f64()),
            (Some(10), Some(6), Some(3.0))
        );
    }

    #[test]
    fn subscriber_gets_meta_then_new_samples() {
        let hub = Arc::new(Hub::default());
        let (_dir, path) = start(&hub);
        hub.publish(&json!({"type": "meta", "t": 0.0}));
        let mut s = UnixStream::connect(&path).unwrap();
        writeln!(s, r#"{{"cmd":"subscribe"}}"#).unwrap();
        let mut lines = BufReader::new(s).lines();
        assert_eq!(
            serde_json::from_str::<Value>(&lines.next().unwrap().unwrap()).unwrap()["type"],
            "meta"
        );
        // the subscription is registered once meta is written; publish after that
        hub.publish(&sample(1.0, "cpu", 1));
        assert_eq!(
            serde_json::from_str::<Value>(&lines.next().unwrap().unwrap()).unwrap()["cpu"],
            1
        );
    }

    #[test]
    fn a_subscriber_that_stops_reading_is_dropped() {
        let hub = Arc::new(Hub::default());
        let (_dir, path) = start(&hub);
        let mut s = UnixStream::connect(&path).unwrap();
        writeln!(s, r#"{{"cmd":"subscribe"}}"#).unwrap();
        while hub.state.lock().unwrap().subscribers.is_empty() {
            thread::sleep(Duration::from_millis(5));
        }
        let big = json!({"type": "sample", "t": 1.0, "blob": "x".repeat(64 * 1024)});
        for _ in 0..200 {
            hub.publish(&big);
        }
        assert!(
            hub.state.lock().unwrap().subscribers.is_empty(),
            "a stalled client must not keep a queue"
        );
        // the client sees the stream end rather than hang forever
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut sink = Vec::new();
        assert!(s.read_to_end(&mut sink).is_ok());
    }

    #[test]
    fn full_process_table_on_demand() {
        let hub = Arc::new(Hub::default());
        let (_dir, path) = start(&hub);
        let reply = ask(&path, "processes");
        let me = std::process::id() as u64;
        let rows = reply["rows"].as_array().unwrap();
        assert!(
            rows.iter().any(|r| r["pid"].as_u64() == Some(me)),
            "the full table, not a top N, includes this test process"
        );
    }

    #[test]
    fn process_identities_are_cached_per_client_and_dropped_with_the_process() {
        // the Processes tab asks every second; re-reading every cmdline and exe each time is the cost this avoids
        let f = Fixture::new()
            .file(
                "proc/5/stat",
                "5 (a) S 1 1 1 0 -1 0 0 0 0 0 10 0 0 0 20 0 1 0 7 100000 10 0 0\n",
            )
            .file("proc/5/cmdline", "first\0");
        let mut scanner = None;
        let cmdline = |reply: String| {
            let v: Value = serde_json::from_str(&reply).unwrap();
            v["rows"][0]["cmdline"].as_str().map(str::to_owned)
        };
        assert_eq!(
            cmdline(processes(&mut scanner, &f.sys()).unwrap()).as_deref(),
            Some("first")
        );
        std::fs::write(f.path().join("proc/5/cmdline"), "second\0").unwrap();
        assert_eq!(
            cmdline(processes(&mut scanner, &f.sys()).unwrap()).as_deref(),
            Some("first"),
            "a running instance keeps its cached command line"
        );
        assert_eq!(scanner.as_ref().unwrap().cached_identities(), 1);
        std::fs::remove_dir_all(f.path().join("proc/5")).unwrap();
        processes(&mut scanner, &f.sys()).unwrap();
        assert_eq!(
            scanner.as_ref().unwrap().cached_identities(),
            0,
            "an exited process leaves no entry"
        );
    }

    #[test]
    fn bad_requests_get_an_error_line_not_a_hangup() {
        let hub = Arc::new(Hub::default());
        let (_dir, path) = start(&hub);
        assert!(
            ask(&path, "frobnicate")["error"]
                .as_str()
                .unwrap()
                .contains("unknown command")
        );
        assert!(ask(&path, "meta")["error"].is_string());
    }

    #[test]
    fn refuses_to_steal_a_live_socket_but_replaces_a_stale_one() {
        let hub = Arc::new(Hub::default());
        let (_dir, path) = start(&hub);
        assert!(serve(&path, hub.clone(), Sys::host()).is_err());

        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("s.sock");
        drop(UnixListener::bind(&stale).unwrap());
        assert!(serve(&stale, hub, Sys::host()).is_ok());
    }
}
