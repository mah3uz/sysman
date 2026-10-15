use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use schema::processes::Table;
use serde::Deserialize;

use crate::data::live::Conn;

const BACKOFF_START: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
const PAUSED_POLL: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub enum Event {
    Conn(Conn),
    Table { t: f64, table: Table },
    Skipped,
}

/// Poll period in milliseconds, shared with the shell; 0 pauses polling while no visible tab needs the table.
pub type Interval = Arc<AtomicU64>;

/// Asks for the full process table every `interval` on its own connection, until the receiver is dropped.
pub fn spawn(path: PathBuf, interval: Interval) -> Receiver<Event> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("processes".into())
        .spawn(move || run(&path, &interval, &tx))
        .expect("spawn processes thread");
    rx
}

fn run(path: &Path, interval: &Interval, tx: &Sender<Event>) {
    let mut backoff = BACKOFF_START;
    loop {
        if let Ok(stream) = UnixStream::connect(path) {
            backoff = BACKOFF_START;
            if tx.send(Event::Conn(Conn::Connected)).is_err() {
                return;
            }
            if !poll(stream, interval, tx) {
                return;
            }
        }
        if tx.send(Event::Conn(Conn::Waiting)).is_err() {
            return;
        }
        thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// False once the receiver is gone; a broken socket returns true so the caller reconnects.
fn poll(stream: UnixStream, interval: &Interval, tx: &Sender<Event>) -> bool {
    let Ok(mut writer) = stream.try_clone() else {
        return true;
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        let period = Duration::from_millis(interval.load(Ordering::Relaxed));
        if period.is_zero() {
            thread::sleep(PAUSED_POLL);
            continue;
        }
        let asked = Instant::now();
        line.clear();
        if writer.write_all(b"{\"cmd\":\"processes\"}\n").is_err()
            || !matches!(reader.read_line(&mut line), Ok(n) if n > 0)
        {
            return true;
        }
        if tx.send(parse(&line)).is_err() {
            return false;
        }
        thread::sleep(period.saturating_sub(asked.elapsed()));
    }
}

/// Reads the reply straight into its types: going through a `Value` first cost more than the rest of the poll.
fn parse(line: &str) -> Event {
    #[derive(Deserialize)]
    struct Stamp {
        t: f64,
    }
    match (serde_json::from_str::<Stamp>(line), serde_json::from_str::<Table>(line)) {
        (Ok(Stamp { t }), Ok(table)) => Event::Table { t, table },
        _ => Event::Skipped,
    }
}

/// The newest full process table from the socket.
pub struct ProcessFeed {
    pub conn: Conn,
    /// Daemon time of the scan and the table.
    pub latest: Option<(f64, Table)>,
    pub version: u64,
}

impl Default for ProcessFeed {
    fn default() -> Self {
        Self {
            conn: Conn::Waiting,
            latest: None,
            version: 0,
        }
    }
}

impl ProcessFeed {
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Conn(conn) => self.conn = conn,
            Event::Skipped => {}
            Event::Table { t, table } => {
                self.latest = Some((t, table));
                self.version += 1;
            }
        }
    }

    pub fn table(&self) -> Option<&Table> {
        self.latest.as_ref().map(|(_, table)| table)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use serde_json::json;

    use super::*;

    fn reply(pid: u32) -> String {
        json!({"type": "processes", "t": 5.0, "count": 1, "threads": 1, "states": {"S": 1},
               "rows": [{"pid": pid, "ppid": 1, "comm": "x", "state": "S", "threads": 1, "rss_bytes": 1,
                         "cpu_pct": 0.0, "cmdline": "x", "start_time": 9}]})
        .to_string()
    }

    /// Answers `answers` requests on one connection, then hangs up.
    fn serve(listener: &UnixListener, pids: &[u32]) {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut stream = stream;
        for &pid in pids {
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            assert_eq!(request, "{\"cmd\":\"processes\"}\n");
            writeln!(stream, "{}", reply(pid)).unwrap();
        }
    }

    fn next_table(rx: &Receiver<Event>) -> u32 {
        loop {
            match rx.recv_timeout(Duration::from_secs(10)).expect("event") {
                Event::Table { table, .. } => return table.rows[0].pid,
                Event::Skipped => panic!("reply did not parse"),
                Event::Conn(_) => {}
            }
        }
    }

    #[test]
    fn a_reply_without_its_time_or_rows_is_skipped() {
        assert!(matches!(parse(&reply(7)), Event::Table { t: 5.0, ref table } if table.rows[0].pid == 7));
        assert!(matches!(parse(r#"{"error": "no rates yet"}"#), Event::Skipped));
        assert!(matches!(
            parse(r#"{"count": 0, "threads": 0, "states": {}, "rows": []}"#),
            Event::Skipped
        ));
    }

    #[test]
    fn polls_repeatedly_and_reconnects_after_the_daemon_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sysman.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let rx = spawn(path.clone(), Arc::new(AtomicU64::new(10)));
        serve(&listener, &[1, 2]);
        assert_eq!((next_table(&rx), next_table(&rx)), (1, 2));
        drop(listener);
        std::fs::remove_file(&path).unwrap();

        let listener = UnixListener::bind(&path).unwrap();
        serve(&listener, &[3]);
        assert_eq!(next_table(&rx), 3);
    }
}
