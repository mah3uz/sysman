use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde_json::{Map, Value, json};

use crate::collector::Spec;
use crate::config::Config;
use crate::pool::{Outcome, Pool, Reply, Req};
use crate::sys::Sys;

struct Module {
    spec: &'static Spec,
    every: u64,
    last_error: Option<String>,
}

pub struct Daemon {
    config: Config,
    base: u64,
    modules: Vec<Module>,
    failed: BTreeMap<String, String>,
    pool: Pool,
}

impl Daemon {
    pub fn new(config: Config, sys: Sys, specs: &'static [Spec]) -> Result<Self> {
        let unknown: Vec<_> = config
            .modules
            .keys()
            .filter(|n| !specs.iter().any(|s| s.name == *n))
            .collect();
        if !unknown.is_empty() {
            let known: Vec<_> = specs.iter().map(|s| s.name).collect();
            bail!("config names unknown modules: {unknown:?}; available: {known:?}");
        }
        let base = config.general.interval;
        let (mut modules, mut collectors, mut failed) = (Vec::new(), Vec::new(), BTreeMap::new());
        for spec in specs {
            let mc = config.modules.get(spec.name);
            if mc.is_some_and(|m| !m.enabled) {
                continue;
            }
            let interval = mc.and_then(|m| m.interval).unwrap_or(spec.default_interval);
            let options = mc.map(|m| m.options.clone()).unwrap_or_default();
            match (spec.build)(options, sys.clone()) {
                Ok(collector) => {
                    // a module cannot run more often than the base tick
                    let every = ((interval as f64 / base as f64).round() as u64).max(1);
                    modules.push(Module {
                        spec,
                        every,
                        last_error: None,
                    });
                    collectors.push((spec.name, collector));
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    eprintln!("{}: disabled, failed to start: {msg}", spec.name);
                    failed.insert(spec.name.to_string(), msg);
                }
            }
        }
        Ok(Self {
            config,
            base,
            modules,
            failed,
            pool: Pool::start(collectors),
        })
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    fn deadline(&self) -> Instant {
        Instant::now() + Duration::from_secs_f64(self.base as f64 * 0.75)
    }

    pub fn meta_record(&mut self) -> Value {
        let now = unix_now();
        let all: Vec<usize> = (0..self.modules.len()).collect();
        let outcomes = self.pool.call(&all, Req::Meta, self.deadline(), now);
        let mut modules = Map::new();
        for (idx, outcome) in outcomes {
            let m = &self.modules[idx];
            let info = match outcome {
                Outcome::Done(Reply::Meta(info)) => info,
                _ => None,
            };
            modules.insert(
                m.spec.name.into(),
                json!({"interval_s": m.every * self.base, "info": info}),
            );
        }
        let mut record = stamp("meta", now);
        record.insert(
            "config".into(),
            json!({"general": self.config.general, "retention": self.config.retention}),
        );
        record.insert("modules".into(), modules.into());
        record.insert("failed_modules".into(), json!(self.failed));
        record.into()
    }

    /// Samples the modules due on `tick`, or every module when `tick` is `None`.
    /// Returns `None` when nothing produced data or an error.
    pub fn sample_record(&mut self, tick: Option<u64>) -> Option<Value> {
        let now = unix_now();
        let due: Vec<usize> = (0..self.modules.len())
            .filter(|&i| tick.is_none_or(|t| t % self.modules[i].every == 0))
            .collect();
        let outcomes = self.pool.call(&due, Req::Sample, self.deadline(), now);

        let mut record = stamp("sample", now);
        let mut errors = Map::new();
        for (idx, outcome) in outcomes {
            let m = &mut self.modules[idx];
            let result = match outcome {
                Outcome::Done(Reply::Sample(r)) => r,
                Outcome::Done(Reply::Meta(_)) => unreachable!("sample request answered with meta"),
                Outcome::Timeout => Err(format!("timeout: no reply within {:.1} s", self.base as f64 * 0.75)),
                Outcome::Busy { since } => Err(format!("busy: still running since {}", local_time(since))),
            };
            match result {
                Ok(data) => {
                    m.last_error = None;
                    if let Some(data) = data {
                        record.insert(m.spec.name.into(), data);
                    }
                }
                Err(msg) => {
                    if m.last_error.as_ref() != Some(&msg) {
                        eprintln!("{}: {msg}", m.spec.name);
                        m.last_error = Some(msg.clone());
                    }
                    errors.insert(m.spec.name.into(), msg.into());
                }
            }
        }
        if record.len() == 4 && errors.is_empty() {
            return None;
        }
        if !errors.is_empty() {
            record.insert("errors".into(), errors.into());
        }
        Some(record.into())
    }
}

pub fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before 1970")
        .as_secs_f64()
}

fn zoned(t: f64) -> jiff::Zoned {
    Timestamp::from_millisecond((t * 1000.0) as i64)
        .expect("time in range")
        .to_zoned(TimeZone::system())
}

pub fn local_time(t: f64) -> String {
    zoned(t).strftime("%Y-%m-%dT%H:%M:%S%z").to_string()
}

pub fn local_date(t: f64) -> Date {
    zoned(t).date()
}

fn stamp(kind: &str, t: f64) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("type".into(), kind.into());
    m.insert("schema".into(), schema::SCHEMA.into());
    m.insert("t".into(), json!((t * 1000.0).round() / 1000.0));
    m.insert("time".into(), local_time(t).into());
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Fixture;

    const ONLY_PSI: &[Spec] = &[Spec {
        name: "pressure",
        default_interval: 2,
        snapshot: false,
        build: |o, sys| {
            Ok(Box::new(crate::collectors::pressure::Pressure::new(
                crate::collector::options(o)?,
                sys,
            )?))
        },
    }];

    fn psi() -> Fixture {
        Fixture::new().file("proc/pressure/cpu", "some avg10=1.00 avg60=2.00 avg300=0 total=0\n")
    }

    #[test]
    fn unknown_module_stops_startup() {
        let config = Config::parse("[modules.presure]\nenabled = true").unwrap();
        let err = Daemon::new(config, psi().sys(), ONLY_PSI).err().unwrap().to_string();
        assert!(err.contains("presure"), "{err}");
    }

    #[test]
    fn module_that_cannot_start_is_listed_not_fatal() {
        let mut d = Daemon::new(Config::parse("").unwrap(), Fixture::new().sys(), ONLY_PSI).unwrap();
        let meta = d.meta_record();
        assert!(meta["failed_modules"]["pressure"].as_str().unwrap().contains("PSI"));
    }

    #[test]
    fn output_round_trips_through_the_schema_crate() {
        // the app reads these records with the schema types; a field the types drop would be lost to it
        let f = psi();
        let mut d = Daemon::new(Config::parse("").unwrap(), f.sys(), ONLY_PSI).unwrap();
        for record in [d.meta_record(), d.sample_record(None).unwrap()] {
            let typed: schema::Record = serde_json::from_value(record.clone()).unwrap();
            assert_eq!(serde_json::to_value(typed).unwrap(), record);
        }
    }

    #[test]
    fn slow_modules_run_only_on_their_ticks() {
        let config = Config::parse("[general]\ninterval = 2\n[modules.pressure]\ninterval = 10").unwrap();
        let f = psi();
        let mut d = Daemon::new(config, f.sys(), ONLY_PSI).unwrap();
        assert!(d.sample_record(Some(3)).is_none(), "nothing due, so no line is written");
        assert!(d.sample_record(Some(5)).unwrap().get("pressure").is_some());
    }
}
