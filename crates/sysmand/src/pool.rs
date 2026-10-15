use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Instant;

use serde_json::Value;

use crate::collector::Collector;

#[derive(Debug, Clone, Copy)]
pub enum Req {
    Sample,
    Meta,
}

#[derive(Debug)]
pub enum Reply {
    Sample(Result<Option<Value>, String>),
    Meta(Option<Value>),
}

#[derive(Debug)]
pub enum Outcome {
    Done(Reply),
    /// No reply before the deadline; a late reply will be discarded.
    Timeout,
    /// The previous request has not returned, so no new one was sent.
    Busy {
        since: f64,
    },
}

struct Msg {
    idx: usize,
    id: u64,
    reply: Reply,
}

struct Worker {
    tx: Sender<(u64, Req)>,
    /// Request id and send time of a request with no reply yet.
    pending: Option<(u64, f64)>,
}

/// One thread per collector, so a collector stuck in a syscall or driver call
/// holds up only itself.
pub struct Pool {
    workers: Vec<Worker>,
    rx: Receiver<Msg>,
    next_id: u64,
}

impl Pool {
    pub fn start(collectors: Vec<(&'static str, Box<dyn Collector>)>) -> Self {
        let (out, rx) = mpsc::channel();
        let workers = collectors
            .into_iter()
            .enumerate()
            .map(|(idx, (name, collector))| {
                let (tx, requests) = mpsc::channel();
                let out = out.clone();
                thread::Builder::new()
                    .name(format!("mod-{name}"))
                    .spawn(move || serve(idx, collector, requests, out))
                    .expect("spawning a collector thread");
                Worker { tx, pending: None }
            })
            .collect();
        Self {
            workers,
            rx,
            next_id: 0,
        }
    }

    /// Send `req` to each worker in `due` and wait for replies until `deadline`.
    /// Returns one outcome per index in `due`, in the same order.
    pub fn call(&mut self, due: &[usize], req: Req, deadline: Instant, now: f64) -> Vec<(usize, Outcome)> {
        while let Ok(msg) = self.rx.try_recv() {
            self.workers[msg.idx].pending = None;
        }

        let mut outcomes: HashMap<usize, Outcome> = HashMap::new();
        let mut waiting: HashMap<usize, u64> = HashMap::new();
        for &idx in due {
            let worker = &mut self.workers[idx];
            if let Some((_, since)) = worker.pending {
                outcomes.insert(idx, Outcome::Busy { since });
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            if worker.tx.send((id, req)).is_err() {
                outcomes.insert(
                    idx,
                    Outcome::Done(Reply::Sample(Err("collector thread is gone".into()))),
                );
                continue;
            }
            worker.pending = Some((id, now));
            waiting.insert(idx, id);
        }

        while !waiting.is_empty() {
            match self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(msg) => {
                    self.workers[msg.idx].pending = None;
                    if waiting.get(&msg.idx) == Some(&msg.id) {
                        waiting.remove(&msg.idx);
                        outcomes.insert(msg.idx, Outcome::Done(msg.reply));
                    }
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        for idx in waiting.into_keys() {
            outcomes.insert(idx, Outcome::Timeout);
        }
        due.iter()
            .map(|idx| (*idx, outcomes.remove(idx).expect("every due index has an outcome")))
            .collect()
    }
}

fn serve(idx: usize, mut collector: Box<dyn Collector>, requests: Receiver<(u64, Req)>, out: Sender<Msg>) {
    for (id, req) in requests {
        let reply = match req {
            Req::Sample => Reply::Sample(guard(|| collector.sample()).and_then(|r| r.map_err(|e| format!("{e:#}")))),
            Req::Meta => Reply::Meta(guard(|| collector.meta()).ok().flatten()),
        };
        if out.send(Msg { idx, id, reply }).is_err() {
            break;
        }
    }
}

fn guard<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|payload| {
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        format!("panic: {msg}")
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use anyhow::Result;
    use serde_json::json;

    use super::*;

    struct Fixed(Value);
    impl Collector for Fixed {
        fn sample(&mut self) -> Result<Option<Value>> {
            Ok(Some(self.0.clone()))
        }
    }

    /// Sleeps for the given number of milliseconds on each call, taken from a shared knob.
    struct Slow(Arc<AtomicU64>);
    impl Collector for Slow {
        fn sample(&mut self) -> Result<Option<Value>> {
            thread::sleep(Duration::from_millis(self.0.load(Ordering::SeqCst)));
            Ok(Some(json!("slow")))
        }
    }

    struct Panics;
    impl Collector for Panics {
        fn sample(&mut self) -> Result<Option<Value>> {
            panic!("boom")
        }
    }

    fn soon(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    #[test]
    fn a_hung_collector_does_not_hold_up_the_others() {
        let delay = Arc::new(AtomicU64::new(10_000));
        let mut pool = Pool::start(vec![
            ("fast", Box::new(Fixed(json!(1)))),
            ("hung", Box::new(Slow(delay))),
        ]);
        let started = Instant::now();
        let out = pool.call(&[0, 1], Req::Sample, soon(200), 0.0);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the tick waited on the hung collector"
        );
        assert!(matches!(&out[0], (0, Outcome::Done(Reply::Sample(Ok(Some(v))))) if *v == json!(1)));
        assert!(matches!(out[1], (1, Outcome::Timeout)));

        // still stuck on the next tick: reported busy, and not sent a second request to queue up
        let out = pool.call(&[0, 1], Req::Sample, soon(200), 7.0);
        assert!(matches!(out[1], (1, Outcome::Busy { since }) if since == 0.0));
    }

    #[test]
    fn a_late_reply_is_discarded_and_frees_the_collector() {
        let delay = Arc::new(AtomicU64::new(300));
        let mut pool = Pool::start(vec![("slow", Box::new(Slow(delay.clone())))]);
        assert!(matches!(
            pool.call(&[0], Req::Sample, soon(50), 0.0)[0].1,
            Outcome::Timeout
        ));
        thread::sleep(Duration::from_millis(400));
        delay.store(0, Ordering::SeqCst);
        // the stale reply from the first call must not be mistaken for this call's reply
        let out = pool.call(&[0], Req::Sample, soon(1000), 1.0);
        assert!(matches!(&out[0].1, Outcome::Done(Reply::Sample(Ok(Some(_))))));
    }

    #[test]
    fn a_panic_becomes_an_error_and_the_collector_keeps_serving() {
        let mut pool = Pool::start(vec![("bad", Box::new(Panics))]);
        for _ in 0..2 {
            let out = pool.call(&[0], Req::Sample, soon(1000), 0.0);
            assert!(matches!(&out[0].1, Outcome::Done(Reply::Sample(Err(e))) if e.contains("boom")));
        }
    }
}
