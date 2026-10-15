use anyhow::Result;
use serde_json::Value;

use crate::collector::{Collector, NoOptions};
use crate::sys::Sys;

/// Whether the privileged helper answers, and which of its sources work, so the app knows what to offer.
pub struct Helper {
    sys: Sys,
}

impl Helper {
    pub fn new(_: NoOptions, sys: Sys) -> Self {
        Self { sys }
    }
}

impl Collector for Helper {
    fn sample(&mut self) -> Result<Option<Value>> {
        let client = self.sys.helper();
        let mut out = schema::helper::Helper {
            state: "stopped".into(),
            version: None,
            features: Default::default(),
            error: None,
        };
        if client.present() {
            match client.status() {
                Ok(status) => {
                    out.state = "running".into();
                    out.version = Some(status.version);
                    out.features = status.features;
                }
                Err(e) => {
                    out.state = "error".into();
                    out.error = Some(format!("{e:#}"));
                }
            }
        }
        Ok(Some(serde_json::to_value(out)?))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    fn sample(f: &Fixture) -> Value {
        let v = Helper::new(NoOptions {}, f.sys()).sample().unwrap().unwrap();
        assert_schema::<schema::helper::Helper>(&v);
        v
    }

    #[test]
    fn no_socket_is_stopped_not_an_error() {
        // the helper is off by default; that is the normal state, not a failure to report
        assert_eq!(sample(&Fixture::new()), json!({"state": "stopped"}));
    }

    #[test]
    fn running_helper_reports_version_and_features() {
        let f = Fixture::new();
        f.fake_helper(
            |_| json!({"version": "0.1.0", "features": {"net": false, "rapl": true}, "errors": {"net": "x"}}),
        );
        assert_eq!(
            sample(&f),
            json!({"state": "running", "version": "0.1.0", "features": {"net": false, "rapl": true}})
        );
    }

    #[test]
    fn a_socket_nobody_answers_is_an_error() {
        let f = Fixture::new();
        let path = f.path().join(crate::helper::SOCKET.trim_start_matches('/'));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let v = sample(&f);
        assert_eq!(v["state"], "error");
        assert!(v["error"].as_str().unwrap().contains("connecting"), "{v}");
    }
}
