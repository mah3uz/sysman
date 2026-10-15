use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use schema::storage::Smart;
use serde_json::Value;

pub const INTERVAL: Duration = Duration::from_secs(600);
const SMARTCTL_TIMEOUT: Duration = Duration::from_secs(30);

/// The latest reading of every drive, kept across passes for drives skipped while asleep.
#[derive(Debug, Default)]
pub struct Latest {
    pub drives: BTreeMap<String, Smart>,
    /// Why some or all drives could not be read in the last pass.
    pub error: Option<String>,
}

pub fn spawn(root: PathBuf, latest: Arc<Mutex<Latest>>) -> std::io::Result<()> {
    std::thread::Builder::new().name("smart".into()).spawn(move || {
        loop {
            let previous = latest.lock().expect("smart lock").drives.clone();
            let (drives, error) = pass(&root, previous);
            *latest.lock().expect("smart lock") = Latest { drives, error };
            std::thread::sleep(INTERVAL);
        }
    })?;
    Ok(())
}

fn pass(root: &Path, mut drives: BTreeMap<String, Smart>) -> (BTreeMap<String, Smart>, Option<String>) {
    let block = root.join("sys/block");
    let names: Vec<String> = std::fs::read_dir(&block)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| nvme_controller(n).is_some() || is_sata(n))
        .collect();
    drives.retain(|name, _| names.contains(name));
    let mut errors = Vec::new();
    let mut smartctl_missing = false;
    for name in names {
        // a runtime-suspended device (the SCSI disk, or the NVMe controller's PCI function) would be resumed by opening it
        let suspended = ["device/power/runtime_status", "device/device/power/runtime_status"]
            .iter()
            .any(|leaf| std::fs::read_to_string(block.join(&name).join(leaf)).is_ok_and(|s| s.trim() == "suspended"));
        if suspended {
            continue;
        }
        let read = match nvme_controller(&name) {
            Some(ctrl) => read_nvme(&root.join("dev").join(ctrl)).map(Some),
            None if smartctl_missing => continue,
            None => match read_sata(&root.join("dev").join(&name)) {
                Err(e)
                    if e.downcast_ref::<std::io::Error>().map(|e| e.kind()) == Some(std::io::ErrorKind::NotFound) =>
                {
                    smartctl_missing = true;
                    errors.push("smartctl is not installed, so SATA drives are skipped".to_string());
                    continue;
                }
                other => other,
            },
        };
        match read {
            Ok(Some(smart)) => {
                drives.insert(name, smart);
            }
            Ok(None) => {}
            Err(e) => errors.push(format!("{name}: {e:#}")),
        }
    }
    if drives.is_empty() && errors.is_empty() {
        errors.push("no NVMe or SATA drives found".into());
    }
    (drives, (!errors.is_empty()).then(|| errors.join("; ")))
}

/// `nvme0n1` → `nvme0`.
fn nvme_controller(block: &str) -> Option<&str> {
    let rest = block.strip_prefix("nvme")?;
    let (ctrl, ns) = rest.split_once('n')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (digits(ctrl) && digits(ns)).then(|| &block[..4 + ctrl.len()])
}

fn is_sata(block: &str) -> bool {
    block
        .strip_prefix("sd")
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase()))
}

/// `struct nvme_passthru_cmd` from `linux/nvme_ioctl.h`.
#[repr(C)]
#[derive(Default)]
struct NvmeAdminCmd {
    opcode: u8,
    flags: u8,
    rsvd1: u16,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    metadata: u64,
    addr: u64,
    metadata_len: u32,
    data_len: u32,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
    timeout_ms: u32,
    result: u32,
}

/// `_IOWR('N', 0x41, struct nvme_admin_cmd)`.
const NVME_IOCTL_ADMIN_CMD: libc::c_ulong = 0xC048_4E41;
const LOG_LEN: usize = 512;

fn read_nvme(dev: &Path) -> Result<Smart> {
    let file = std::fs::File::open(dev).with_context(|| format!("opening {}", dev.display()))?;
    let mut log = [0u8; LOG_LEN];
    let mut cmd = NvmeAdminCmd {
        opcode: 0x02,
        nsid: 0xFFFF_FFFF,
        addr: log.as_mut_ptr() as u64,
        data_len: LOG_LEN as u32,
        // log page 0x02 (SMART / Health Information); upper half is the dword count minus one
        cdw10: 0x02 | (((LOG_LEN / 4 - 1) as u32) << 16),
        timeout_ms: 5000,
        ..Default::default()
    };
    // SAFETY: cmd is a valid nvme_passthru_cmd whose addr points at `log`, which outlives the call and is data_len bytes.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), NVME_IOCTL_ADMIN_CMD, &mut cmd) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error()).context("Get Log Page");
    }
    if rc > 0 {
        bail!("Get Log Page: NVMe status {rc:#x}");
    }
    Ok(parse_health_log(&log))
}

const CRITICAL_WARNINGS: [&str; 6] = [
    "spare_below_threshold",
    "temperature",
    "reliability_degraded",
    "read_only",
    "volatile_backup_failed",
    "pmr_read_only",
];

/// The NVMe SMART / Health Information log page (0x02).
fn parse_health_log(log: &[u8; LOG_LEN]) -> Smart {
    let u128_at = |at: usize| u128::from_le_bytes(log[at..at + 16].try_into().expect("16 bytes"));
    let count = |at: usize| Some(u64::try_from(u128_at(at)).unwrap_or(u64::MAX));
    // data units are thousands of 512-byte sectors
    let bytes = |at: usize| Some(u64::try_from(u128_at(at).saturating_mul(512_000)).unwrap_or(u64::MAX));
    let kelvin = u16::from_le_bytes([log[1], log[2]]);
    let warning = log[0];
    Smart {
        passed: warning == 0,
        temp_c: (kelvin > 0).then(|| f64::from(kelvin) - 273.0),
        percent_used: Some(log[5]),
        available_spare_pct: Some(log[3]),
        power_on_hours: count(128),
        power_cycles: count(112),
        unsafe_shutdowns: count(144),
        media_errors: count(160),
        data_read_bytes: bytes(32),
        data_written_bytes: bytes(48),
        warnings: CRITICAL_WARNINGS
            .iter()
            .enumerate()
            .filter(|(bit, _)| warning & (1 << bit) != 0)
            .map(|(_, name)| name.to_string())
            .collect(),
    }
}

/// `None` when the drive is in standby: `-n standby` makes smartctl skip it rather than spin it up.
fn read_sata(dev: &Path) -> Result<Option<Smart>> {
    let mut child = Command::new("smartctl")
        .args(["-j", "-n", "standby", "-H", "-A", "-i"])
        .arg(dev)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let started = Instant::now();
    while child.try_wait()?.is_none() {
        if started.elapsed() > SMARTCTL_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("smartctl did not finish in {}s", SMARTCTL_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = child.wait_with_output()?;
    let json: Value = serde_json::from_slice(&out.stdout).context("smartctl output is not JSON")?;
    parse_smartctl(&json)
}

fn parse_smartctl(json: &Value) -> Result<Option<Smart>> {
    let Some(passed) = json["smart_status"]["passed"].as_bool() else {
        let messages: Vec<&str> = json["smartctl"]["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["string"].as_str())
            .collect();
        if messages.iter().any(|m| m.contains("STANDBY")) {
            return Ok(None);
        }
        bail!("smartctl reported no health status: {}", messages.join("; "));
    };
    let attributes = json["ata_smart_attributes"]["table"].as_array();
    let warnings = attributes
        .into_iter()
        .flatten()
        .filter(|a| a["when_failed"].as_str().is_some_and(|w| !w.is_empty()))
        .filter_map(|a| a["name"].as_str().map(str::to_string))
        .collect();
    Ok(Some(Smart {
        passed,
        temp_c: json["temperature"]["current"].as_f64(),
        power_on_hours: json["power_on_time"]["hours"].as_u64(),
        power_cycles: json["power_cycle_count"].as_u64(),
        warnings,
        ..Default::default()
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn health_log() -> [u8; LOG_LEN] {
        let mut log = [0u8; LOG_LEN];
        log[1..3].copy_from_slice(&318u16.to_le_bytes());
        log[3] = 100;
        log[5] = 7;
        log[32..48].copy_from_slice(&2_000u128.to_le_bytes());
        log[48..64].copy_from_slice(&3_000u128.to_le_bytes());
        log[112..128].copy_from_slice(&1_234u128.to_le_bytes());
        log[128..144].copy_from_slice(&5_678u128.to_le_bytes());
        log[144..160].copy_from_slice(&42u128.to_le_bytes());
        log[160..176].copy_from_slice(&0u128.to_le_bytes());
        log
    }

    #[test]
    fn nvme_health_log_fields_sit_at_their_spec_offsets() {
        let s = parse_health_log(&health_log());
        assert!(s.passed);
        assert_eq!(s.temp_c, Some(45.0), "the composite temperature is in kelvin");
        assert_eq!((s.percent_used, s.available_spare_pct), (Some(7), Some(100)));
        assert_eq!(
            (s.power_cycles, s.power_on_hours, s.unsafe_shutdowns),
            (Some(1_234), Some(5_678), Some(42))
        );
        assert_eq!(s.media_errors, Some(0));
        // one data unit is 1000 sectors of 512 bytes, not 1024 and not one sector
        assert_eq!(s.data_read_bytes, Some(2_000 * 512_000));
        assert_eq!(s.data_written_bytes, Some(3_000 * 512_000));
        assert!(s.warnings.is_empty());
    }

    #[test]
    fn nvme_critical_warning_bits_fail_the_drive_and_are_named() {
        let mut log = health_log();
        log[0] = 0b1001;
        let s = parse_health_log(&log);
        assert!(!s.passed);
        assert_eq!(s.warnings, ["spare_below_threshold", "read_only"]);
    }

    #[test]
    fn nvme_counters_beyond_u64_saturate() {
        let mut log = health_log();
        log[48..64].copy_from_slice(&u128::MAX.to_le_bytes());
        assert_eq!(parse_health_log(&log).data_written_bytes, Some(u64::MAX));
    }

    #[test]
    fn smartctl_json_maps_to_smart() {
        let j = json!({
            "smartctl": {"exit_status": 0},
            "smart_status": {"passed": true},
            "temperature": {"current": 34},
            "power_on_time": {"hours": 12000},
            "power_cycle_count": 900,
            "ata_smart_attributes": {"table": [
                {"id": 5, "name": "Reallocated_Sector_Ct", "when_failed": ""},
                {"id": 187, "name": "Reported_Uncorrect", "when_failed": "now"}
            ]}
        });
        let s = parse_smartctl(&j).unwrap().unwrap();
        assert!(s.passed);
        assert_eq!(
            (s.temp_c, s.power_on_hours, s.power_cycles),
            (Some(34.0), Some(12000), Some(900))
        );
        assert_eq!(s.warnings, ["Reported_Uncorrect"]);
    }

    #[test]
    fn smartctl_standby_is_no_reading_not_an_error() {
        // the previous reading must survive, and the drive must not be woken for a new one
        let j = json!({"smartctl": {"exit_status": 2, "messages": [{"string": "Device is in STANDBY mode, exit(2)", "severity": "information"}]}});
        assert_eq!(parse_smartctl(&j).unwrap(), None);
        let j =
            json!({"smartctl": {"exit_status": 2, "messages": [{"string": "Permission denied", "severity": "error"}]}});
        assert!(
            parse_smartctl(&j)
                .unwrap_err()
                .to_string()
                .contains("Permission denied")
        );
    }

    #[test]
    fn block_names_map_to_their_source() {
        assert_eq!(nvme_controller("nvme0n1"), Some("nvme0"));
        assert_eq!(nvme_controller("nvme12n3"), Some("nvme12"));
        assert_eq!(nvme_controller("nvme0n1p2"), None, "partitions are not drives");
        assert!(is_sata("sda") && is_sata("sdab"));
        assert!(!is_sata("sda1") && !is_sata("sr0"));
    }

    #[test]
    fn asleep_and_vanished_drives_keep_or_drop_their_reading() {
        let root = tempfile::tempdir().unwrap();
        let dev = root.path().join("sys/block/nvme0n1/device/power");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("runtime_status"), "suspended\n").unwrap();
        let old = Smart {
            passed: true,
            ..Default::default()
        };
        let previous = BTreeMap::from([
            ("nvme0n1".to_string(), old.clone()),
            ("nvme9n1".to_string(), old.clone()),
        ]);
        let (drives, error) = pass(root.path(), previous);
        assert_eq!(drives, BTreeMap::from([("nvme0n1".to_string(), old)]));
        assert_eq!(error, None, "a sleeping drive is not a failure");
    }
}
