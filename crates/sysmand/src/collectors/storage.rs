use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use schema::storage::{Btrfs, Device, DeviceErrors, Drive, StorageMeta};
use serde_json::Value;

use crate::collector::Collector;
use crate::sys::Sys;

pub use sysman_config::modules::storage::Options;

pub struct Storage {
    sys: Sys,
    exclude: Vec<String>,
}

impl Storage {
    pub fn new(options: Options, sys: Sys) -> Result<Self> {
        if sys.list("/sys/block").is_empty() {
            bail!("no block devices in /sys/block");
        }
        Ok(Self {
            sys,
            exclude: options.exclude,
        })
    }

    fn devices(&self) -> Vec<String> {
        let mut names = self.sys.list("/sys/block");
        names.retain(|d| !self.exclude.iter().any(|p| d.starts_with(p.as_str())));
        names
    }
}

impl Collector for Storage {
    fn meta(&mut self) -> Option<Value> {
        let devices = self
            .devices()
            .into_iter()
            .map(|dev| {
                let base = format!("/sys/block/{dev}");
                let drive = Drive {
                    model: self.sys.read_opt(format!("{base}/device/model")),
                    serial: self.sys.read_opt(format!("{base}/device/serial")),
                    firmware: self.sys.read_opt(format!("{base}/device/firmware_rev")),
                    size_bytes: self.sys.int_opt(format!("{base}/size")).map(|s| s as u64 * 512),
                    rotational: self.sys.int_opt(format!("{base}/queue/rotational")) == Some(1),
                };
                (dev, drive)
            })
            .collect();
        let note = if self.sys.helper().present() {
            "SMART wear and health come from sysman-helper"
        } else {
            "SMART wear and health need root and are not collected"
        };
        let meta = StorageMeta {
            devices,
            note: note.into(),
        };
        serde_json::to_value(meta).ok()
    }

    fn sample(&mut self) -> Result<Option<Value>> {
        let devices = self
            .devices()
            .into_iter()
            .map(|dev| {
                let state = self.sys.read_opt(format!("/sys/block/{dev}/device/state"));
                (dev, Device { state })
            })
            .collect();

        let mut btrfs = BTreeMap::new();
        for uuid in self.sys.list("/sys/fs/btrfs").into_iter().filter(|n| n.contains('-')) {
            let fs = format!("/sys/fs/btrfs/{uuid}");
            let mut device_errors = BTreeMap::new();
            for devid in self.sys.list(format!("{fs}/devinfo")) {
                let info = format!("{fs}/devinfo/{devid}");
                let errors = self
                    .sys
                    .read_opt(format!("{info}/error_stats"))
                    .and_then(|text| parse_error_stats(&text).ok())
                    .unwrap_or_default();
                let missing = self.sys.int_opt(format!("{info}/missing")).map(|m| u8::from(m != 0));
                device_errors.insert(devid, DeviceErrors { missing, errors });
            }
            let label = self.sys.read_opt(format!("{fs}/label")).filter(|l| !l.is_empty());
            btrfs.insert(uuid, Btrfs { label, device_errors });
        }
        Ok(Some(serde_json::to_value(schema::storage::Storage {
            devices,
            btrfs,
            smart: self
                .sys
                .helper()
                .read()
                .map(|(_, r)| r.smart.clone())
                .unwrap_or_default(),
        })?))
    }
}

fn parse_error_stats(text: &str) -> Result<BTreeMap<String, u64>> {
    text.lines()
        .map(|line| {
            let (key, value) = line
                .split_once(char::is_whitespace)
                .with_context(|| format!("{line:?}"))?;
            Ok((
                key.to_string(),
                value.trim().parse().with_context(|| format!("{line:?}"))?,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{Fixture, assert_schema};

    const UUID: &str = "37f6d04c-3e11-418d-b7b4-c8cc11706c28";
    const CLEAN: &str = "write_errs 0\nread_errs 0\nflush_errs 0\ncorruption_errs 0\ngeneration_errs 0\n";

    fn host() -> Fixture {
        Fixture::new()
            .file("sys/block/nvme0n1/device/state", "live\n")
            .file(
                "sys/block/nvme0n1/device/model",
                "Samsung SSD 990 PRO 2TB                 \n",
            )
            .file("sys/block/nvme0n1/device/serial", "S6Z2NU0XA21241J     \n")
            .file("sys/block/nvme0n1/device/firmware_rev", "4B2QJXD7\n")
            .file("sys/block/nvme0n1/size", "3907029168\n")
            .file("sys/block/nvme0n1/queue/rotational", "0\n")
            .file("sys/block/sda/queue/rotational", "1\n")
            .file("sys/block/zram0/size", "8388608\n")
            .file("sys/block/loop0/size", "0\n")
            .file(&format!("sys/fs/btrfs/{UUID}/label"), "\n")
            .file(&format!("sys/fs/btrfs/{UUID}/devinfo/1/error_stats"), CLEAN)
            .file(&format!("sys/fs/btrfs/{UUID}/devinfo/1/missing"), "0\n")
            .file("sys/fs/btrfs/features/x", "1\n")
    }

    fn collector(f: &Fixture) -> Storage {
        Storage::new(Options::default(), f.sys()).unwrap()
    }

    #[test]
    fn meta_inventories_drives_and_skips_excluded() {
        let f = host();
        let v = collector(&f).meta().unwrap();
        assert_schema::<StorageMeta>(&v);
        let d = &v["devices"];
        assert_eq!(d.as_object().unwrap().keys().collect::<Vec<_>>(), ["nvme0n1", "sda"]);
        assert_eq!(d["nvme0n1"]["model"], "Samsung SSD 990 PRO 2TB");
        assert_eq!(d["nvme0n1"]["size_bytes"], 3907029168u64 * 512);
        assert_eq!(d["nvme0n1"]["rotational"], false);
        // a drive that exposes no identity must show null, not an empty or zero value
        assert_eq!(d["sda"]["model"], Value::Null);
        assert_eq!(d["sda"]["size_bytes"], Value::Null);
        assert_eq!(d["sda"]["rotational"], true);
    }

    #[test]
    fn nvme_controller_state_is_in_the_sample() {
        let f = host().file("sys/block/nvme0n1/device/state", "dead\n");
        let v = collector(&f).sample().unwrap().unwrap();
        assert_schema::<schema::storage::Storage>(&v);
        assert_eq!(v["devices"]["nvme0n1"]["state"], "dead");
        assert_eq!(v["devices"]["sda"]["state"], Value::Null);
        assert!(v["devices"].get("zram0").is_none());
    }

    #[test]
    fn btrfs_errors_are_keyed_by_uuid_then_devid() {
        let f = host().file(
            &format!("sys/fs/btrfs/{UUID}/devinfo/1/error_stats"),
            "write_errs 0\nread_errs 3\nflush_errs 0\ncorruption_errs 7\ngeneration_errs 0\n",
        );
        let v = collector(&f).sample().unwrap().unwrap();
        assert_schema::<schema::storage::Storage>(&v);
        let dev = &v["btrfs"][UUID]["device_errors"]["1"];
        assert_eq!(dev["read_errs"], 3);
        assert_eq!(dev["corruption_errs"], 7);
        assert_eq!(dev["missing"], 0);
        // an unlabelled filesystem has no label, and non-filesystem entries in the directory are ignored
        assert_eq!(v["btrfs"][UUID]["label"], Value::Null);
        assert_eq!(v["btrfs"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn missing_raid_member_is_reported_even_without_error_stats() {
        // the point of the module: a degraded raid must show which device is gone
        let f = host()
            .file(&format!("sys/fs/btrfs/{UUID}/label"), "pool\n")
            .file(&format!("sys/fs/btrfs/{UUID}/devinfo/2/missing"), "1\n");
        let v = collector(&f).sample().unwrap().unwrap();
        assert_schema::<schema::storage::Storage>(&v);
        let fs = &v["btrfs"][UUID];
        assert_eq!(fs["label"], "pool");
        assert_eq!(fs["device_errors"]["2"]["missing"], 1);
        assert_eq!(fs["device_errors"]["1"]["missing"], 0);
    }

    #[test]
    fn malformed_error_stats_drops_that_devids_counters_only() {
        let f = host()
            .file(
                &format!("sys/fs/btrfs/{UUID}/devinfo/1/error_stats"),
                "write_errs many\n",
            )
            .file(&format!("sys/fs/btrfs/{UUID}/devinfo/1/missing"), "1\n")
            .file(
                &format!("sys/fs/btrfs/{UUID}/devinfo/2/error_stats"),
                "write_errs 0\nread_errs 4\n",
            );
        let v = collector(&f).sample().unwrap().unwrap();
        assert_schema::<schema::storage::Storage>(&v);
        assert_eq!(v["devices"]["nvme0n1"]["state"], "live");
        let errs = &v["btrfs"][UUID]["device_errors"];
        assert_eq!(errs["1"], serde_json::json!({"missing": 1}), "no counters are invented");
        assert_eq!(errs["2"]["read_errs"], 4);
    }

    #[test]
    fn no_block_devices_disables_the_module() {
        assert!(Storage::new(Options::default(), Fixture::new().sys()).is_err());
    }

    #[test]
    fn smart_comes_from_the_helper_and_is_absent_without_it() {
        let f = host();
        let before = collector(&f).sample().unwrap().unwrap();
        assert!(
            before.get("smart").is_none(),
            "no helper: the output is as it always was"
        );
        f.fake_helper(
            |_| serde_json::json!({"smart": {"nvme0n1": {"passed": true, "temp_c": 41.0, "percent_used": 3}}}),
        );
        let v = collector(&f).sample().unwrap().unwrap();
        assert_schema::<schema::storage::Storage>(&v);
        assert_eq!(v["smart"]["nvme0n1"]["percent_used"], 3);
        assert_eq!(v["devices"], before["devices"]);
        assert_eq!(
            collector(&f).meta().unwrap()["note"],
            "SMART wear and health come from sysman-helper"
        );
    }
}
