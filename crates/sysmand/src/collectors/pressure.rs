use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use schema::pressure::Stall;
use serde_json::{Value, json};

use crate::collector::{Collector, NoOptions};
use crate::sys::Sys;

pub struct Pressure {
    sys: Sys,
    resources: Vec<String>,
}

impl Pressure {
    pub fn new(_: NoOptions, sys: Sys) -> Result<Self> {
        let resources = sys.list("/proc/pressure");
        if resources.is_empty() {
            bail!("no /proc/pressure (kernel built without PSI, or psi=0)");
        }
        Ok(Self { sys, resources })
    }
}

impl Collector for Pressure {
    fn meta(&mut self) -> Option<Value> {
        Some(json!({"unit": "percent of time tasks were stalled", "some": "at least one task", "full": "all tasks"}))
    }

    fn sample(&mut self) -> Result<Option<Value>> {
        let mut out: schema::pressure::Pressure = BTreeMap::new();
        for res in &self.resources {
            let text = self.sys.read(format!("/proc/pressure/{res}"))?;
            for line in text.lines() {
                let (scope, stall) = parse_line(line).with_context(|| format!("/proc/pressure/{res}: {line:?}"))?;
                out.entry(res.clone()).or_default().insert(scope, stall);
            }
        }
        Ok(Some(serde_json::to_value(out)?))
    }
}

fn parse_line(line: &str) -> Option<(String, Stall)> {
    let mut words = line.split_whitespace();
    let scope = words.next()?.to_string();
    let (mut avg10, mut avg60) = (None, None);
    for field in words {
        match field.split_once('=')? {
            ("avg10", v) => avg10 = v.parse().ok(),
            ("avg60", v) => avg60 = v.parse().ok(),
            _ => {}
        }
    }
    Some((
        scope,
        Stall {
            avg10: avg10?,
            avg60: avg60?,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    #[test]
    fn reads_every_resource_and_scope() {
        let f = Fixture::new()
            .file(
                "proc/pressure/cpu",
                "some avg10=1.50 avg60=0.25 avg300=0.10 total=123\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n",
            )
            .file("proc/pressure/irq", "full avg10=3.00 avg60=2.00 avg300=1.00 total=9\n");
        let mut c = Pressure::new(NoOptions {}, f.sys()).unwrap();
        let v = c.sample().unwrap().unwrap();
        assert_schema::<schema::pressure::Pressure>(&v);
        assert_eq!(v["cpu"]["some"]["avg10"], 1.5);
        assert_eq!(v["cpu"]["full"]["avg60"], 0.0);
        // irq has only `full`; inventing a `some` would chart a value the kernel never reported
        assert_eq!(v["irq"].as_object().unwrap().keys().collect::<Vec<_>>(), ["full"]);
    }

    #[test]
    fn malformed_line_is_an_error_not_a_zero() {
        let f = Fixture::new().file("proc/pressure/cpu", "some avg10=x avg60=0.25\n");
        let mut c = Pressure::new(NoOptions {}, f.sys()).unwrap();
        assert!(c.sample().is_err());
    }

    #[test]
    fn kernel_without_psi_disables_the_module() {
        assert!(Pressure::new(NoOptions {}, Fixture::new().sys()).is_err());
    }
}
