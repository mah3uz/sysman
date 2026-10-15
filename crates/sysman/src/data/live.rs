use schema::series::{escape, flatten};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use schema::{Meta, Record, Sample};
use serde::Deserialize;
use serde_json::Value;

use crate::data::{net, source};

/// How long a series ring reaches back.
pub const WINDOW_S: f64 = 30. * 60.;
const RING_CAPACITY: usize = (WINDOW_S as usize) / 2;
const BACKOFF_START: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
const ENVELOPE: [&str; 5] = ["type", "schema", "t", "time", "errors"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conn {
    Waiting,
    Connected,
}

#[derive(Debug)]
pub enum Event {
    Conn(Conn),
    Record { record: Record, points: Vec<(String, f64)> },
    Skipped,
}

/// `None` when `XDG_RUNTIME_DIR` is unset or not absolute, as in the daemon.
pub fn socket_path() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    dir.is_absolute().then(|| dir.join("sysman.sock"))
}

/// Subscribes on a background thread until the receiver is dropped.
pub fn spawn(path: PathBuf) -> Receiver<Event> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("live".into())
        .spawn(move || run(&path, &tx))
        .expect("spawn live thread");
    rx
}

fn run(path: &Path, tx: &Sender<Event>) {
    let mut backoff = BACKOFF_START;
    loop {
        if let Ok(stream) = UnixStream::connect(path) {
            backoff = BACKOFF_START;
            if tx.send(Event::Conn(Conn::Connected)).is_err() {
                return;
            }
            if !subscribe(stream, tx) {
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
fn subscribe(mut stream: UnixStream, tx: &Sender<Event>) -> bool {
    if stream.write_all(b"{\"cmd\":\"subscribe\"}\n").is_err() {
        return true;
    }
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else {
            return true;
        };
        if tx.send(parse(&line)).is_err() {
            return false;
        }
    }
    true
}

fn parse(line: &str) -> Event {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Event::Skipped;
    };
    let Ok(record) = Record::deserialize(&value) else {
        return Event::Skipped;
    };
    let mut points = Vec::new();
    if let (Record::Sample(_), Some(fields)) = (&record, value.as_object()) {
        for (name, v) in fields.iter().filter(|(k, _)| !ENVELOPE.contains(&k.as_str())) {
            flatten(v, &mut format!("/{}", escape(name)), &mut points);
        }
    }
    Event::Record { record, points }
}

/// What the app knows from the socket: the newest value of every module and the last 30 min of every series.
pub struct Live {
    pub conn: Conn,
    pub meta: Option<Meta>,
    /// Each module as last reported; a sample carries only the modules due on its tick. Shared, so a render can
    /// hold it without copying every module.
    pub latest: Option<Rc<Sample>>,
    pub skipped: u64,
    /// Kept here, not on the Network screen, so it counts while that tab is unmounted.
    pub net_session: net::Session,
    series: HashMap<String, VecDeque<(f64, f64)>>,
    /// Sample times, trimmed as the rings are, so the front is how far back every ring reaches.
    times: VecDeque<f64>,
    /// Sample time at which live work last resumed after a pause.
    resumed_at: Option<f64>,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            conn: Conn::Waiting,
            meta: None,
            latest: None,
            skipped: 0,
            net_session: net::Session::default(),
            series: HashMap::new(),
            times: VecDeque::new(),
            resumed_at: None,
        }
    }
}

impl Live {
    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Conn(conn) => self.conn = conn,
            Event::Skipped => self.skipped += 1,
            Event::Record {
                record: Record::Meta(meta),
                ..
            } => self.meta = Some(meta),
            Event::Record {
                record: Record::Sample(sample),
                points,
            } => {
                let t = sample.t;
                trim(&mut self.times, |&t0| t0, t);
                self.times.push_back(t);
                for (path, v) in points {
                    let ring = self.series.entry(path).or_default();
                    trim(ring, |&(t0, _)| t0, t);
                    ring.push_back((t, v));
                }
                if let Some(n) = &sample.net {
                    self.net_session.observe(n);
                }
                self.merge(*sample);
            }
        }
    }

    pub fn resumed(&mut self) {
        self.resumed_at = self.latest.as_ref().map(|s| s.t);
    }

    /// Whether history a screen read at `read_at` for `span_s` is due again; see [`source::reload_due`].
    pub fn reload_due(&self, read_at: f64, now: f64, span_s: f64, every_s: f64) -> bool {
        let rings_from = self.times.front().copied().unwrap_or(f64::INFINITY);
        source::reload_due(read_at, now, span_s, every_s, self.resumed_at, rings_from)
    }

    /// `(t, value)` oldest first.
    pub fn series(&self, path: &str) -> Option<&VecDeque<(f64, f64)>> {
        self.series.get(path)
    }

    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.series.keys().map(String::as_str)
    }

    pub fn value(&self, path: &str) -> Option<f64> {
        self.series(path)?.back().map(|&(_, v)| v)
    }

    fn merge(&mut self, sample: Sample) {
        let Some(latest) = &mut self.latest else {
            self.latest = Some(Rc::new(sample));
            return;
        };
        let latest = Rc::make_mut(latest);
        macro_rules! keep {
            ($($module:ident),*) => {$(
                if sample.$module.is_some() {
                    latest.$module = sample.$module;
                }
            )*};
        }
        keep!(
            cgroups, cpu, disk, fs, gpu, helper, hwmon, memory, net, netinfo, power, pressure, processes, storage,
            system
        );
        latest.schema = sample.schema;
        latest.t = sample.t;
        latest.time = sample.time;
        latest.errors = sample.errors;
    }
}

/// Drops what falls out of the window before a point at `t` is added, or what would exceed the capacity.
fn trim<T>(ring: &mut VecDeque<T>, time: impl Fn(&T) -> f64, t: f64) {
    while ring.front().is_some_and(|p| time(p) < t - WINDOW_S) || ring.len() >= RING_CAPACITY {
        ring.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::time::Instant;

    use serde_json::json;

    use super::*;

    fn meta_line() -> String {
        json!({"type": "meta", "schema": 3, "t": 1.0, "time": "x", "config": {},
               "modules": {"cpu": {"interval_s": 2, "info": null}}, "failed_modules": {}})
        .to_string()
    }

    fn sample_line(t: f64, pct: f64) -> String {
        json!({"type": "sample", "schema": 3, "t": t, "time": "x",
               "hwmon": {"k10temp/0000:00:18.3": {"temp": {"Tctl": pct}}}})
        .to_string()
    }

    fn next(rx: &Receiver<Event>) -> Event {
        rx.recv_timeout(Duration::from_secs(5)).expect("event")
    }

    /// Serves one subscriber: checks the request, writes `lines`, then hangs up.
    fn serve(listener: &UnixListener, lines: &[String]) {
        let (stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        BufReader::new(&stream).read_line(&mut request).unwrap();
        assert_eq!(request, "{\"cmd\":\"subscribe\"}\n");
        let mut stream = stream;
        for line in lines {
            writeln!(stream, "{line}").unwrap();
        }
    }

    #[test]
    fn reconnects_after_the_daemon_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sysman.sock");
        let rx = spawn(path.clone());
        assert!(matches!(next(&rx), Event::Conn(Conn::Waiting)), "no daemon yet");

        let listener = UnixListener::bind(&path).unwrap();
        serve(&listener, &[meta_line(), sample_line(2.0, 40.), sample_line(4.0, 41.)]);
        drop(listener);
        std::fs::remove_file(&path).unwrap();

        let mut live = Live::default();
        let until = |live: &mut Live, done: &dyn Fn(&Live) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done(live) {
                assert!(Instant::now() < deadline, "timed out");
                live.apply(next(&rx));
            }
        };
        until(&mut live, &|l| l.conn == Conn::Waiting && l.series.len() == 1);
        assert!(live.meta.is_some());
        let tctl = "/hwmon/k10temp~10000:00:18.3/temp/Tctl";
        assert_eq!(
            live.series(tctl).unwrap().iter().copied().collect::<Vec<_>>(),
            [(2.0, 40.), (4.0, 41.)]
        );

        let listener = UnixListener::bind(&path).unwrap();
        serve(&listener, &[meta_line(), sample_line(6.0, 42.)]);
        until(&mut live, &|l| l.value(tctl) == Some(42.));
        assert_eq!(live.series(tctl).unwrap().len(), 3);
    }

    #[test]
    fn a_malformed_line_is_counted_and_the_stream_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sysman.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let rx = spawn(path);
        serve(
            &listener,
            &[
                "{not json".into(),
                json!({"type": "sample"}).to_string(),
                sample_line(2.0, 40.),
            ],
        );
        let mut live = Live::default();
        while live.latest.is_none() {
            live.apply(next(&rx));
        }
        assert_eq!(live.skipped, 2);
        assert_eq!(live.conn, Conn::Connected);
    }

    #[test]
    fn rings_keep_thirty_minutes_and_modules_keep_their_last_value() {
        let mut live = Live::default();
        for i in 0..1000 {
            let line = if i == 0 {
                json!({"type": "sample", "schema": 3, "t": 0.0, "time": "x",
                       "pressure": {"memory": {"some": {"avg10": 1.0, "avg60": 1.0}}}})
                .to_string()
            } else {
                sample_line(f64::from(i) * 2., 1.)
            };
            live.apply(parse(&line));
        }
        let ring = live.series("/hwmon/k10temp~10000:00:18.3/temp/Tctl").unwrap();
        assert!(ring.front().unwrap().0 >= 1998. - WINDOW_S);
        assert!(ring.len() <= RING_CAPACITY);
        let latest = live.latest.as_ref().unwrap();
        assert!(
            latest.pressure.is_some(),
            "a module absent from later ticks keeps its last value"
        );
        assert_eq!(latest.t, 1998.);
    }
}
