use std::cmp::Ordering;
use std::collections::BTreeMap;

use schema::hwmon::{Hwmon, HwmonMeta};
use schema::series::escape;

use super::{Domain, NetDevice, domain_of};

/// The thing a node describes, so its page can find the live series and related items that belong to it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Item {
    Cpu,
    /// One L3 group, by index.
    Ccd(usize),
    /// PCI slot.
    Gpu(String),
    Memory,
    /// hwmon chip of one module's sensor.
    Dimm(String),
    /// `card1-DP-1`.
    Display(String),
    /// Block device name.
    Drive(String),
    /// Mount point.
    Volume(String),
    /// sysfs supply name.
    Supply(String),
    /// hwmon chip (`name/device`).
    Chip(String),
    Net(String),
    /// sysfs port, `8-1.2`.
    Usb(String),
    /// PCI slot.
    Pci(String),
}

/// `/a/b/c` from raw keys, escaped as series paths are.
pub fn path(parts: &[&str]) -> String {
    parts.iter().map(|p| format!("/{}", escape(p))).collect()
}

pub fn core_path(cpu: usize) -> String {
    path(&["cpu", "core_pct", &cpu.to_string()])
}

/// The controller's hwmon chip of an NVMe namespace: `nvme0n1` → `nvme/nvme0`.
pub fn nvme_chip(drive: &str) -> Option<String> {
    let digits: String = drive
        .strip_prefix("nvme")?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (!digits.is_empty()).then(|| format!("nvme/nvme{digits}"))
}

/// The disk a partition belongs to, from its name alone: `nvme1n1p2` → `nvme1n1`, `sda3` → `sda`.
pub fn partition_parent(name: &str) -> Option<&str> {
    let stem = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if stem.len() == name.len() {
        return None;
    }
    if ["nvme", "mmcblk", "loop", "nbd"].iter().any(|p| name.starts_with(p)) {
        let base = stem.strip_suffix('p')?;
        return base.ends_with(|c: char| c.is_ascii_digit()).then_some(base);
    }
    ["sd", "vd", "hd", "xvd"]
        .iter()
        .any(|p| stem.starts_with(p) && stem.len() > p.len())
        .then_some(stem)
}

/// The disk holding a filesystem's device, through device-mapper and partitions as sysfs resolved them, else by name.
pub fn disk_of(device: &str, holders: &BTreeMap<String, String>) -> Option<String> {
    if let Some(disk) = holders.get(device) {
        return Some(disk.clone());
    }
    let name = device.strip_prefix("/dev/")?;
    Some(partition_parent(name).unwrap_or(name).to_owned())
}

/// hwmon chips that measure the item itself, out of every chip in the sample.
pub fn item_chips<'a>(item: &Item, chips: &'a [String], nets: &[NetDevice]) -> Vec<&'a str> {
    let device = |c: &'a String| c.split_once('/').map_or("", |(_, d)| d);
    chips
        .iter()
        .filter(|c| match item {
            Item::Drive(d) => nvme_chip(d).as_deref() == Some(c.as_str()),
            Item::Gpu(slot) => device(c) == slot,
            Item::Memory => domain_of(c) == Domain::Memory,
            Item::Dimm(chip) | Item::Chip(chip) => *c == chip,
            Item::Net(name) => {
                let phy = nets.iter().find(|n| &n.name == name).and_then(|n| n.phy.as_deref());
                c.split('/').next() == Some(name.as_str()) || phy.is_some_and(|p| device(c) == p)
            }
            _ => false,
        })
        .map(String::as_str)
        .collect()
}

fn prefixes(item: &Item, chips: &[&str]) -> Vec<String> {
    let chip_prefix = |c: &&str| format!("{}/", path(&["hwmon", c]));
    let mut out: Vec<String> = match item {
        Item::Cpu | Item::Ccd(_) => [
            "/cpu/total_pct",
            "/cpu/core_pct/",
            "/cpu/avg_mhz",
            "/cpu/temp",
            "/cpu/power_w",
        ]
        .map(str::to_owned)
        .into(),
        Item::Gpu(slot) => vec![format!("{}/", path(&["gpu", "devices", slot]))],
        Item::Memory => ["used_bytes", "cache_bytes", "swap_used_bytes", "zram/"]
            .iter()
            .map(|f| format!("/memory/{f}"))
            .collect(),
        Item::Drive(d) => vec![
            format!("{}/", path(&["disk", d])),
            format!("{}/", path(&["storage", "smart", d])),
        ],
        Item::Volume(m) => vec![format!("{}/", path(&["fs", m]))],
        Item::Net(n) => vec![format!("{}/", path(&["net", "interfaces", n]))],
        Item::Supply(s) => vec![format!("{}/", path(&["power", s]))],
        Item::Dimm(_) | Item::Chip(_) | Item::Display(_) | Item::Usb(_) | Item::Pci(_) => Vec::new(),
    };
    out.extend(chips.iter().map(chip_prefix));
    out
}

/// The series an item's page charts, out of every path known: its own module's, and its hwmon chips'.
/// Running totals and inode counts are left out; the page shows those as current values.
pub fn charted<'a>(item: &Item, chips: &[&str], paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let prefixes = prefixes(item, chips);
    let mut out: Vec<String> = paths
        .into_iter()
        .filter(|p| prefixes.iter().any(|pre| p.starts_with(pre.as_str())))
        .filter(|p| !p.ends_with("_total") && !p.contains("/inodes_"))
        .map(str::to_owned)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// One reading of an hwmon chip, of any kind, with the limits worth drawing.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub kind: String,
    pub label: String,
    pub path: String,
    pub value: Option<f64>,
    pub max: Option<f64>,
    pub crit: Option<f64>,
}

fn kind_rank(kind: &str) -> u8 {
    match kind {
        "temp" => 0,
        "fan" => 1,
        "power" => 2,
        "curr" => 3,
        "in" => 4,
        _ => 5,
    }
}

/// `in2` before `in10`.
pub fn natural(a: &str, b: &str) -> Ordering {
    let split = |s: &str| {
        let stem = s.trim_end_matches(|c: char| c.is_ascii_digit());
        (stem.to_owned(), s[stem.len()..].parse::<u64>().ok())
    };
    split(a).cmp(&split(b)).then_with(|| a.cmp(b))
}

/// Every reading of `chip`, temperatures first; limits only where the chip set a believable one.
pub fn chip_readings(chip: &str, hwmon: Option<&Hwmon>, meta: Option<&HwmonMeta>) -> Vec<Reading> {
    let mut out = Vec::new();
    for (kind, labels) in hwmon.and_then(|h| h.get(chip)).into_iter().flatten() {
        for (label, value) in labels {
            let limits = meta.and_then(|m| m.limits.get(chip)?.get(kind)?.get(label));
            let limit = |name: &str| {
                limits
                    .and_then(|l| l.get(name).copied())
                    .filter(|&v| v > 0. && (kind != "temp" || (20. ..=150.).contains(&v)))
            };
            out.push(Reading {
                kind: kind.clone(),
                label: label.clone(),
                path: path(&["hwmon", chip, kind, label]),
                value: *value,
                max: limit("max"),
                crit: limit("crit"),
            });
        }
    }
    out.sort_by(|a, b| {
        kind_rank(&a.kind)
            .cmp(&kind_rank(&b.kind))
            .then_with(|| natural(&a.label, &b.label))
    });
    out
}

/// PCI slots along a sysfs device path, root port first.
pub fn pci_slots(path: &str) -> Vec<&str> {
    path.split('/')
        .filter(|c| {
            let b = c.as_bytes();
            b.len() == 12 && b[4] == b':' && b[7] == b':' && b[10] == b'.'
        })
        .collect()
}

/// The hub a USB device hangs from: `8-1.2` → `8-1` → `usb8`; root hubs have none.
pub fn usb_parent(port: &str) -> Option<String> {
    if port.starts_with("usb") {
        return None;
    }
    match port.rsplit_once('.') {
        Some((hub, _)) => Some(hub.to_owned()),
        None => port.split_once('-').map(|(bus, _)| format!("usb{bus}")),
    }
}

/// The page that says more about a PCI function: the GPU, network interface or drive it is.
pub fn pci_related(
    slot: &str,
    gpus: &[String],
    nets: &[NetDevice],
    block_pci: &BTreeMap<String, String>,
) -> Option<Item> {
    if gpus.iter().any(|g| g == slot) {
        return Some(Item::Gpu(slot.to_owned()));
    }
    if let Some(n) = nets
        .iter()
        .find(|n| n.bus.as_deref().and_then(|b| b.strip_prefix("PCI ")) == Some(slot))
    {
        return Some(Item::Net(n.name.clone()));
    }
    block_pci
        .iter()
        .find(|(_, s)| *s == slot)
        .map(|(disk, _)| Item::Drive(disk.clone()))
}

/// Marketing names for common ratios, else the ratio to one.
pub fn aspect_label(w: u32, h: u32) -> Option<String> {
    if w == 0 || h == 0 {
        return None;
    }
    let r = f64::from(w) / f64::from(h);
    let known = [
        (16. / 9., "16:9"),
        (16. / 10., "16:10"),
        (64. / 27., "21:9"),
        (32. / 9., "32:9"),
        (4. / 3., "4:3"),
        (5. / 4., "5:4"),
        (3. / 2., "3:2"),
        (1., "1:1"),
    ];
    Some(
        known
            .iter()
            .find(|(k, _)| (r / k - 1.).abs() < 0.015)
            .map_or_else(|| format!("{r:.2}:1"), |(_, l)| (*l).to_owned()),
    )
}

/// Pixels per inch along the diagonal.
pub fn ppi(w: u32, h: u32, w_mm: u32, h_mm: u32) -> Option<f64> {
    let inches = super::diagonal_in(w_mm, h_mm);
    (inches > 0. && w > 0).then(|| f64::from(w).hypot(f64::from(h)) / inches)
}

/// "just now", "4 min ago", "2 h ago".
pub fn ago(seconds: f64) -> String {
    let s = seconds.max(0.) as u64;
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn net(name: &str, bus: Option<&str>, phy: Option<&str>) -> NetDevice {
        NetDevice {
            name: name.into(),
            mac: None,
            state: None,
            speed_mbps: None,
            driver: None,
            mtu: None,
            wireless: phy.is_some(),
            signal_dbm: None,
            bus: bus.map(str::to_owned),
            phy: phy.map(str::to_owned),
        }
    }

    const PATHS: &[&str] = &[
        "/cpu/total_pct",
        "/cpu/core_pct/0",
        "/cpu/core_mhz/0",
        "/cpu/breakdown_pct/user",
        "/cpu/temps_c/Tccd1",
        "/disk/nvme0n1/read_bps",
        "/disk/nvme0n1/read_bytes_total",
        "/disk/nvme0n10/read_bps",
        "/disk/nvme1n1/read_bps",
        "/storage/smart/nvme0n1/temp_c",
        "/hwmon/nvme~1nvme0/temp/Composite",
        "/hwmon/nvme~1nvme1/temp/Composite",
        "/hwmon/amdgpu~10000:7a:00.0/temp/edge",
        "/hwmon/spd5118~17-0050/temp/temp1",
        "/hwmon/mt7925_phy0~1phy0/temp/temp1",
        "/hwmon/enp12s0~10000:0c:00.0/temp/PHY Temperature",
        "/gpu/devices/0000:7a:00.0/util_pct",
        "/gpu/devices/0000:01:00.0/util_pct",
        "/net/interfaces/wlp10s0/rx_bps",
        "/net/interfaces/wlp10s0/rx_bytes_total",
        "/net/interfaces/enp12s0/tx_bps",
        "/memory/used_bytes",
        "/memory/page_faults_ps",
        "/memory/zram/zram0/orig_bytes",
        "/fs/~1boot/used_bytes",
        "/fs/~1boot/inodes_free",
        "/fs/~1/used_bytes",
    ];

    fn chips() -> Vec<String> {
        [
            "nvme/nvme0",
            "nvme/nvme1",
            "amdgpu/0000:7a:00.0",
            "spd5118/7-0050",
            "mt7925_phy0/phy0",
            "enp12s0/0000:0c:00.0",
            "k10temp/0000:00:18.3",
        ]
        .map(str::to_owned)
        .into()
    }

    fn series(item: Item, nets: &[NetDevice]) -> Vec<String> {
        let chips = chips();
        let mine = item_chips(&item, &chips, nets);
        charted(&item, &mine, PATHS.iter().copied())
    }

    #[test]
    fn a_drive_charts_its_own_io_smart_and_controller_temperature_only() {
        assert_eq!(
            series(Item::Drive("nvme0n1".into()), &[]),
            [
                "/disk/nvme0n1/read_bps",
                "/hwmon/nvme~1nvme0/temp/Composite",
                "/storage/smart/nvme0n1/temp_c",
            ],
            "not nvme0n10's I/O nor nvme1's sensor, and running totals are values, not charts"
        );
    }

    #[test]
    fn an_interface_charts_its_traffic_and_the_sensors_of_its_card() {
        let nets = [
            net("wlp10s0", Some("PCI 0000:0a:00.0"), Some("phy0")),
            net("enp12s0", Some("PCI 0000:0c:00.0"), None),
        ];
        assert_eq!(
            series(Item::Net("wlp10s0".into()), &nets),
            ["/hwmon/mt7925_phy0~1phy0/temp/temp1", "/net/interfaces/wlp10s0/rx_bps"],
            "a Wi-Fi card's sensor is named after its phy, not the interface"
        );
        assert_eq!(
            series(Item::Net("enp12s0".into()), &nets),
            [
                "/hwmon/enp12s0~10000:0c:00.0/temp/PHY Temperature",
                "/net/interfaces/enp12s0/tx_bps"
            ]
        );
    }

    #[test]
    fn a_gpu_charts_its_slot_and_the_hwmon_chip_on_that_slot() {
        assert_eq!(
            series(Item::Gpu("0000:7a:00.0".into()), &[]),
            [
                "/gpu/devices/0000:7a:00.0/util_pct",
                "/hwmon/amdgpu~10000:7a:00.0/temp/edge"
            ]
        );
    }

    #[test]
    fn memory_charts_usage_zram_and_module_temperatures() {
        assert_eq!(
            series(Item::Memory, &[]),
            [
                "/hwmon/spd5118~17-0050/temp/temp1",
                "/memory/used_bytes",
                "/memory/zram/zram0/orig_bytes"
            ],
            "paging rates belong on other screens"
        );
    }

    #[test]
    fn the_processor_charts_load_frequency_and_temperature_but_not_per_core_clocks() {
        assert_eq!(
            series(Item::Cpu, &[]),
            ["/cpu/core_pct/0", "/cpu/temps_c/Tccd1", "/cpu/total_pct"]
        );
    }

    #[test]
    fn a_volume_charts_its_own_mount_only() {
        assert_eq!(series(Item::Volume("/".into()), &[]), ["/fs/~1/used_bytes"]);
    }

    #[test]
    fn nvme_namespaces_map_to_their_controller_chip() {
        assert_eq!(nvme_chip("nvme0n1").as_deref(), Some("nvme/nvme0"));
        assert_eq!(nvme_chip("nvme12n3").as_deref(), Some("nvme/nvme12"));
        assert_eq!(nvme_chip("sda"), None);
    }

    #[test]
    fn filesystems_find_their_disk_through_partitions_and_device_mapper() {
        let holders = BTreeMap::from([("/dev/mapper/root".to_owned(), "nvme1n1".to_owned())]);
        assert_eq!(disk_of("/dev/nvme1n1p2", &holders).as_deref(), Some("nvme1n1"));
        assert_eq!(disk_of("/dev/sda3", &holders).as_deref(), Some("sda"));
        assert_eq!(disk_of("/dev/mmcblk0p1", &holders).as_deref(), Some("mmcblk0"));
        assert_eq!(disk_of("/dev/mapper/root", &holders).as_deref(), Some("nvme1n1"));
        assert_eq!(
            disk_of("/dev/nvme0n1", &holders).as_deref(),
            Some("nvme0n1"),
            "a whole-disk filesystem"
        );
        assert_eq!(disk_of("tmpfs", &holders), None);
        assert_eq!(
            partition_parent("nvme0n1"),
            None,
            "a namespace number is not a partition"
        );
    }

    #[test]
    fn chip_readings_keep_every_kind_in_natural_order_with_believable_limits() {
        let hwmon: Hwmon = serde_json::from_value(serde_json::json!({
            "nct/x": {
                "in": {"in10": 1.1, "in2": 0.9},
                "fan": {"fan1": 900.0},
                "temp": {"SYSTIN": 31.0, "Sensor 1": 40.0}
            }
        }))
        .unwrap();
        let meta: HwmonMeta = serde_json::from_value(serde_json::json!({
            "units": {}, "note": "",
            "limits": {"nct/x": {
                "temp": {"SYSTIN": {"max": 80.0, "crit": 125.0}, "Sensor 1": {"max": 65261.85}},
                "in": {"in2": {"max": 0.0}, "in10": {"max": 1.744}}
            }}
        }))
        .unwrap();
        let r = chip_readings("nct/x", Some(&hwmon), Some(&meta));
        let labels: Vec<&str> = r.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["SYSTIN", "Sensor 1", "fan1", "in2", "in10"]);
        assert_eq!((r[0].max, r[0].crit), (Some(80.), Some(125.)));
        assert_eq!(r[1].max, None, "an unset limit reads absurdly high");
        assert_eq!(r[3].max, None, "a zero voltage limit is unset");
        assert_eq!(r[4].max, Some(1.744));
        assert_eq!(r[0].path, "/hwmon/nct~1x/temp/SYSTIN");
    }

    #[test]
    fn device_trees_walk_up_to_the_root() {
        let path = "/sys/devices/pci0000:00/0000:00:01.4/0000:03:00.0/nvme/nvme0";
        assert_eq!(pci_slots(path), ["0000:00:01.4", "0000:03:00.0"]);
        assert_eq!(usb_parent("8-1.2.4").as_deref(), Some("8-1.2"));
        assert_eq!(usb_parent("8-1").as_deref(), Some("usb8"));
        assert_eq!(usb_parent("usb8"), None);
    }

    #[test]
    fn a_pci_function_links_to_the_page_that_explains_it() {
        let nets = [net("enp12s0", Some("PCI 0000:0c:00.0"), None)];
        let block = BTreeMap::from([("nvme0n1".to_owned(), "0000:03:00.0".to_owned())]);
        let gpus = ["0000:01:00.0".to_owned()];
        let rel = |s: &str| pci_related(s, &gpus, &nets, &block);
        assert_eq!(rel("0000:01:00.0"), Some(Item::Gpu("0000:01:00.0".into())));
        assert_eq!(rel("0000:0c:00.0"), Some(Item::Net("enp12s0".into())));
        assert_eq!(rel("0000:03:00.0"), Some(Item::Drive("nvme0n1".into())));
        assert_eq!(rel("0000:00:01.4"), None);
    }

    #[test]
    fn display_geometry() {
        assert_eq!(aspect_label(3840, 2160).as_deref(), Some("16:9"));
        assert_eq!(aspect_label(3440, 1440).as_deref(), Some("21:9"));
        assert_eq!(aspect_label(1920, 1200).as_deref(), Some("16:10"));
        assert_eq!(aspect_label(1000, 300).as_deref(), Some("3.33:1"));
        assert_eq!(aspect_label(0, 10), None);
        let p = ppi(3840, 2160, 600, 340).unwrap();
        assert!((p - 162.).abs() < 1., "a 27-inch 4K panel is about 162 ppi, got {p}");
    }

    #[test]
    fn ages_read_as_people_say_them() {
        assert_eq!(ago(5.), "just now");
        assert_eq!(ago(130.), "2 min ago");
        assert_eq!(ago(7300.), "2 h ago");
        assert_eq!(ago(-3.), "just now", "a clock step back is not the future");
    }
}
