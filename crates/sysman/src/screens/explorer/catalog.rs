use gpui::Hsla;
use gpui_kit_assets::IconName;

use crate::charts::format::Unit;
use crate::charts::trend::YScale;
use crate::data::history::ProcField;
use crate::data::net;
use crate::data::source::{Combine, Source};
use crate::ui::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hue {
    Green,
    Orange,
    Red,
    Blue,
    Teal,
    Purple,
    Pink,
    Indigo,
}

impl Hue {
    /// Lines that come from a pattern take these in turn.
    pub const CYCLE: [Hue; 8] = [
        Self::Blue,
        Self::Orange,
        Self::Green,
        Self::Purple,
        Self::Teal,
        Self::Pink,
        Self::Red,
        Self::Indigo,
    ];
    /// Compared processes keep their slot's colour for as long as they stay selected.
    pub const PROCESSES: [Hue; 8] = [
        Self::Purple,
        Self::Blue,
        Self::Orange,
        Self::Green,
        Self::Pink,
        Self::Teal,
        Self::Red,
        Self::Indigo,
    ];

    pub fn color(self, t: &Theme) -> Hsla {
        match self {
            Self::Green => t.green,
            Self::Orange => t.orange,
            Self::Red => t.red,
            Self::Blue => t.blue,
            Self::Teal => t.teal,
            Self::Purple => t.purple,
            Self::Pink => t.pink,
            Self::Indigo => t.indigo,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Processor,
    Memory,
    Network,
    Disk,
    Gpu,
    Sensors,
    Energy,
    Processes,
}

impl Group {
    pub const ALL: [Group; 8] = [
        Self::Processor,
        Self::Memory,
        Self::Network,
        Self::Disk,
        Self::Gpu,
        Self::Sensors,
        Self::Energy,
        Self::Processes,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Processor => "Processor",
            Self::Memory => "Memory",
            Self::Network => "Network",
            Self::Disk => "Disk",
            Self::Gpu => "GPU",
            Self::Sensors => "Sensors",
            Self::Energy => "Energy",
            Self::Processes => "Processes",
        }
    }

    pub fn icon(self) -> IconName {
        match self {
            Self::Processor => IconName::Cpu,
            Self::Memory => IconName::MemoryStick,
            Self::Network => IconName::Globe,
            Self::Disk => IconName::HardDrive,
            Self::Gpu => IconName::Monitor,
            Self::Sensors => IconName::Thermometer,
            Self::Energy => IconName::Zap,
            Self::Processes => IconName::List,
        }
    }
}

/// Which stored series a line reads.
#[derive(Clone, Copy)]
pub enum Pick {
    Path(&'static str),
    Sum(fn(&str) -> bool),
    Max(fn(&str) -> bool),
    /// One line per matching path, labelled by what the function returns.
    Each(fn(&str) -> Option<String>),
    Pressure,
}

pub struct SeriesDef {
    pub label: &'static str,
    pub hue: Hue,
    pub pick: Pick,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Machine,
    Processes(ProcField),
}

pub struct ChartDef {
    pub id: &'static str,
    pub title: &'static str,
    pub group: Group,
    pub unit: Unit,
    pub y: YScale,
    pub scope: Scope,
    pub series: &'static [SeriesDef],
}

#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub label: String,
    pub hue: Hue,
    pub source: Source,
}

impl ChartDef {
    /// The lines this chart draws from the known series; empty when none of them exist.
    pub fn resolve(&self, known: &[&str]) -> Vec<Resolved> {
        let mut out = Vec::new();
        for s in self.series {
            let found = |keep: fn(&str) -> bool| {
                let mut paths: Vec<String> = known.iter().filter(|p| keep(p)).map(|p| p.to_string()).collect();
                paths.sort();
                paths.dedup();
                paths
            };
            let mut line = |paths: Vec<String>, combine| {
                if !paths.is_empty() {
                    out.push(Resolved {
                        label: s.label.into(),
                        hue: s.hue,
                        source: Source::new(paths, combine),
                    });
                }
            };
            match s.pick {
                Pick::Path(path) => line(found_exact(known, path), Combine::Sum),
                Pick::Sum(keep) => line(found(keep), Combine::Sum),
                Pick::Max(keep) => line(found(keep), Combine::Max),
                Pick::Pressure => {
                    if known.contains(&PRESSURE[0]) {
                        line(PRESSURE.iter().map(|p| p.to_string()).collect(), Combine::Pressure);
                    }
                }
                Pick::Each(label) => {
                    let mut each: Vec<(String, String)> =
                        known.iter().filter_map(|p| Some((label(p)?, p.to_string()))).collect();
                    each.sort();
                    each.dedup();
                    for (i, (label, path)) in each.into_iter().enumerate() {
                        out.push(Resolved {
                            label,
                            hue: Hue::CYCLE[i % Hue::CYCLE.len()],
                            source: Source::new(vec![path], Combine::Sum),
                        });
                    }
                }
            }
        }
        out
    }

    /// Process charts are always offered; machine charts only when the history has their series.
    pub fn available(&self, known: &[&str]) -> bool {
        matches!(self.scope, Scope::Processes(_)) || !self.resolve(known).is_empty()
    }
}

fn found_exact(known: &[&str], path: &str) -> Vec<String> {
    if known.contains(&path) {
        vec![path.to_string()]
    } else {
        Vec::new()
    }
}

const PRESSURE: [&str; 2] = ["/pressure/memory/some/avg10", "/memory/swap_in_pages_ps"];

/// Series keys escape `/` and `~` as in JSON pointers.
pub fn unescape(key: &str) -> String {
    key.replace("~1", "/").replace("~0", "~")
}

/// The single path segment between `prefix` and `suffix`.
fn key<'a>(path: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    path.strip_prefix(prefix)?
        .strip_suffix(suffix)
        .filter(|k| !k.is_empty() && !k.contains('/'))
}

/// Compressed swap, loop files, device-mapper and RAID devices would count the same bytes twice.
fn physical(device: &str) -> bool {
    !["zram", "loop", "dm-", "md", "ram", "sr"]
        .iter()
        .any(|p| device.starts_with(p))
}

fn disk(path: &str, suffix: &str) -> bool {
    key(path, "/disk/", suffix).is_some_and(physical)
}

fn disk_read(p: &str) -> bool {
    disk(p, "/read_bps")
}
fn disk_write(p: &str) -> bool {
    disk(p, "/write_bps")
}
fn disk_read_iops(p: &str) -> bool {
    disk(p, "/read_iops")
}
fn disk_write_iops(p: &str) -> bool {
    disk(p, "/write_iops")
}
fn disk_read_await(p: &str) -> bool {
    disk(p, "/read_await_ms")
}
fn disk_write_await(p: &str) -> bool {
    disk(p, "/write_await_ms")
}
fn disk_busy(p: &str) -> bool {
    disk(p, "/busy_pct")
}
fn net_rx(p: &str) -> bool {
    key(p, "/net/interfaces/", "/rx_bps").is_some_and(net::counted)
}
fn net_tx(p: &str) -> bool {
    key(p, "/net/interfaces/", "/tx_bps").is_some_and(net::counted)
}
fn kernel(p: &str) -> bool {
    matches!(
        p,
        "/memory/slab_bytes" | "/memory/kernel_stack_bytes" | "/memory/page_tables_bytes"
    )
}
fn zram(p: &str) -> bool {
    key(p, "/memory/zram/", "/mem_used_bytes").is_some()
}
fn gpu_temp(p: &str) -> bool {
    key(p, "/gpu/devices/", "/temp_c").is_some()
}

/// `GPU 01:00.0` for `0000:01:00.0`.
fn gpu_label(p: &str, suffix: &str) -> Option<String> {
    let id = key(p, "/gpu/devices/", suffix)?;
    Some(format!("GPU {}", id.strip_prefix("0000:").unwrap_or(id)))
}
fn gpu_util(p: &str) -> Option<String> {
    gpu_label(p, "/util_pct")
}
fn gpu_power(p: &str) -> Option<String> {
    gpu_label(p, "/power_w")
}
fn gpu_memory(p: &str) -> Option<String> {
    gpu_label(p, "/vram_used_bytes")
}
fn ccd_temp(p: &str) -> Option<String> {
    let k = p.strip_prefix("/cpu/temps_c/").filter(|k| !k.contains('/'))?;
    Some(match k.strip_prefix("Tccd") {
        Some(n) => format!("CCD {n}"),
        None => unescape(k),
    })
}
fn storage_temp(p: &str) -> Option<String> {
    key(p, "/hwmon/nvme~1", "/temp/Composite").map(unescape)
}
fn memory_temp(p: &str) -> Option<String> {
    key(p, "/hwmon/spd5118~1", "/temp/temp1").map(|k| format!("DIMM {}", unescape(k)))
}
fn fan(p: &str) -> Option<String> {
    let mut parts = p.strip_prefix("/hwmon/")?.split('/');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(_), Some("fan"), Some(name), None) => Some(unescape(name)),
        _ => None,
    }
}
fn battery_charge(p: &str) -> Option<String> {
    key(p, "/power/", "/capacity").map(unescape)
}
fn battery_power(p: &str) -> Option<String> {
    key(p, "/power/", "/power_w").map(unescape)
}

const fn line(label: &'static str, hue: Hue, pick: Pick) -> SeriesDef {
    SeriesDef { label, hue, pick }
}

const fn machine(
    id: &'static str,
    title: &'static str,
    group: Group,
    unit: Unit,
    y: YScale,
    series: &'static [SeriesDef],
) -> ChartDef {
    ChartDef {
        id,
        title,
        group,
        unit,
        y,
        scope: Scope::Machine,
        series,
    }
}

const fn process(id: &'static str, title: &'static str, unit: Unit, field: ProcField) -> ChartDef {
    ChartDef {
        id,
        title,
        group: Group::Processes,
        unit,
        y: YScale::Zero,
        scope: Scope::Processes(field),
        series: &[],
    }
}

const PERCENT: YScale = YScale::Fixed(0., 100.);
const ZERO: YScale = YScale::Zero;

use Hue::*;
use Pick::*;

/// Every chart the Explorer offers, in sidebar order.
pub const CHARTS: &[ChartDef] = &[
    machine(
        "cpu",
        "Total CPU",
        Group::Processor,
        Unit::Percent,
        PERCENT,
        &[line("Total CPU", Green, Path("/cpu/total_pct"))],
    ),
    machine(
        "cpu_breakdown",
        "CPU breakdown",
        Group::Processor,
        Unit::Percent,
        ZERO,
        &[
            line("User", Blue, Path("/cpu/breakdown_pct/user")),
            line("System", Red, Path("/cpu/breakdown_pct/system")),
            line("I/O wait", Orange, Path("/cpu/breakdown_pct/iowait")),
            line("Nice", Teal, Path("/cpu/breakdown_pct/nice")),
        ],
    ),
    machine(
        "load",
        "Load averages",
        Group::Processor,
        Unit::Count,
        ZERO,
        &[
            line("1 min", Blue, Path("/system/load/0")),
            line("5 min", Teal, Path("/system/load/1")),
            line("15 min", Orange, Path("/system/load/2")),
        ],
    ),
    machine(
        "cpu_mhz",
        "CPU frequency (MHz)",
        Group::Processor,
        Unit::Count,
        ZERO,
        &[line("Average", Green, Path("/cpu/avg_mhz"))],
    ),
    machine(
        "cpu_stall",
        "CPU stall",
        Group::Processor,
        Unit::Percent,
        ZERO,
        &[line("Some tasks", Orange, Path("/pressure/cpu/some/avg10"))],
    ),
    machine(
        "tasks",
        "Tasks",
        Group::Processor,
        Unit::Count,
        ZERO,
        &[
            line("Processes", Blue, Path("/processes/count")),
            line("Running", Green, Path("/system/procs_running")),
            line("Blocked", Red, Path("/system/procs_blocked")),
        ],
    ),
    machine(
        "pressure",
        "Pressure index",
        Group::Memory,
        Unit::Index,
        PERCENT,
        &[line("Pressure index", Orange, Pressure)],
    ),
    machine(
        "memory",
        "Memory composition",
        Group::Memory,
        Unit::Bytes,
        ZERO,
        &[
            line("App", Blue, Path("/memory/anon_bytes")),
            line("Kernel", Purple, Sum(kernel)),
            line("Compressed", Orange, Sum(zram)),
            line("Cached files", Teal, Path("/memory/cached_bytes")),
        ],
    ),
    machine(
        "memory_used",
        "Memory used",
        Group::Memory,
        Unit::Percent,
        PERCENT,
        &[line("Used", Blue, Path("/memory/used_pct"))],
    ),
    machine(
        "swap",
        "Swap",
        Group::Memory,
        Unit::Bytes,
        ZERO,
        &[line("Swap used", Indigo, Path("/memory/swap_used_bytes"))],
    ),
    machine(
        "memory_stall",
        "Memory stall",
        Group::Memory,
        Unit::Percent,
        ZERO,
        &[line("Some tasks", Orange, Path("/pressure/memory/some/avg10"))],
    ),
    machine(
        "network",
        "Network throughput",
        Group::Network,
        Unit::BytesPerSec,
        ZERO,
        &[line("Download", Green, Sum(net_rx)), line("Upload", Red, Sum(net_tx))],
    ),
    machine(
        "tcp_segments",
        "TCP segments",
        Group::Network,
        Unit::Count,
        ZERO,
        &[
            line("In", Green, Path("/net/tcp/tcp_in_segments_ps")),
            line("Out", Red, Path("/net/tcp/tcp_out_segments_ps")),
        ],
    ),
    machine(
        "tcp_retransmits",
        "TCP retransmits",
        Group::Network,
        Unit::Count,
        ZERO,
        &[line("Retransmits", Orange, Path("/net/tcp/tcp_retransmits_ps"))],
    ),
    machine(
        "sockets",
        "Sockets",
        Group::Network,
        Unit::Count,
        ZERO,
        &[
            line("TCP", Blue, Path("/net/sockets/tcp_in_use")),
            line("UDP", Teal, Path("/net/sockets/udp_in_use")),
        ],
    ),
    machine(
        "disk",
        "Physical disk",
        Group::Disk,
        Unit::BytesPerSec,
        ZERO,
        &[
            line("Read", Blue, Sum(disk_read)),
            line("Write", Orange, Sum(disk_write)),
        ],
    ),
    machine(
        "iops",
        "IOPS",
        Group::Disk,
        Unit::Count,
        ZERO,
        &[
            line("Read", Blue, Sum(disk_read_iops)),
            line("Write", Orange, Sum(disk_write_iops)),
        ],
    ),
    machine(
        "service_time",
        "Service time",
        Group::Disk,
        Unit::Millis,
        ZERO,
        &[
            line("Read", Blue, Max(disk_read_await)),
            line("Write", Orange, Max(disk_write_await)),
        ],
    ),
    machine(
        "disk_util",
        "Disk utilization",
        Group::Disk,
        Unit::Percent,
        PERCENT,
        &[line("Busiest device", Orange, Max(disk_busy))],
    ),
    machine(
        "root_space",
        "Root volume free space",
        Group::Disk,
        Unit::Bytes,
        ZERO,
        &[line("Free", Blue, Path("/fs/~1/avail_bytes"))],
    ),
    machine(
        "io_stall",
        "I/O stall",
        Group::Disk,
        Unit::Percent,
        ZERO,
        &[line("Some tasks", Orange, Path("/pressure/io/some/avg10"))],
    ),
    machine(
        "gpu_util",
        "GPU utilization",
        Group::Gpu,
        Unit::Percent,
        PERCENT,
        &[line("", Green, Each(gpu_util))],
    ),
    machine(
        "gpu_power",
        "GPU power",
        Group::Gpu,
        Unit::Watts,
        ZERO,
        &[line("", Blue, Each(gpu_power))],
    ),
    machine(
        "gpu_memory",
        "GPU memory",
        Group::Gpu,
        Unit::Bytes,
        ZERO,
        &[line("", Teal, Each(gpu_memory))],
    ),
    machine(
        "die_temps",
        "Die temperatures",
        Group::Sensors,
        Unit::Celsius,
        YScale::Fit(10.),
        &[
            line("CPU die", Orange, Path("/cpu/temp_c")),
            line("GPU die", Red, Max(gpu_temp)),
        ],
    ),
    machine(
        "ccd_temps",
        "CPU complex temperatures",
        Group::Sensors,
        Unit::Celsius,
        YScale::Fit(10.),
        &[line("", Orange, Each(ccd_temp))],
    ),
    machine(
        "storage_temps",
        "Storage temperatures",
        Group::Sensors,
        Unit::Celsius,
        YScale::Fit(10.),
        &[line("", Blue, Each(storage_temp))],
    ),
    machine(
        "memory_temps",
        "Memory module temperatures",
        Group::Sensors,
        Unit::Celsius,
        YScale::Fit(10.),
        &[line("", Blue, Each(memory_temp))],
    ),
    machine(
        "fans",
        "Fans (rpm)",
        Group::Sensors,
        Unit::Count,
        ZERO,
        &[line("", Teal, Each(fan))],
    ),
    machine(
        "cpu_power",
        "CPU package power",
        Group::Energy,
        Unit::Watts,
        ZERO,
        &[line("Package", Orange, Path("/cpu/power_w"))],
    ),
    machine(
        "battery_charge",
        "Battery charge",
        Group::Energy,
        Unit::Percent,
        PERCENT,
        &[line("", Green, Each(battery_charge))],
    ),
    machine(
        "battery_power",
        "Battery power",
        Group::Energy,
        Unit::Watts,
        ZERO,
        &[line("", Orange, Each(battery_power))],
    ),
    process("proc_cpu", "Process CPU", Unit::Percent, ProcField::Cpu),
    process("proc_memory", "Resident memory", Unit::Bytes, ProcField::Rss),
    process("proc_threads", "Threads", Unit::Count, ProcField::Threads),
    process("proc_fds", "File descriptors", Unit::Count, ProcField::Fds),
    process("proc_read", "Process disk read", Unit::BytesPerSec, ProcField::Read),
    process("proc_write", "Process disk write", Unit::BytesPerSec, ProcField::Write),
];

/// Read through the privileged helper; when it is missing the sidebar says so instead of listing the chart.
pub const HELPER_CHART: &str = "cpu_power";

pub fn chart(id: &str) -> Option<&'static ChartDef> {
    CHARTS.iter().find(|c| c.id == id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Overview,
    Cpu,
    Memory,
    Disk,
    Network,
    Thermals,
    Gpu,
}

impl Preset {
    pub const ALL: [Preset; 7] = [
        Self::Overview,
        Self::Cpu,
        Self::Memory,
        Self::Disk,
        Self::Network,
        Self::Thermals,
        Self::Gpu,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Cpu => "CPU",
            Self::Memory => "Memory",
            Self::Disk => "Disk",
            Self::Network => "Network",
            Self::Thermals => "Thermals",
            Self::Gpu => "GPU",
        }
    }

    pub fn charts(self) -> &'static [&'static str] {
        match self {
            Self::Overview => &[
                "cpu",
                "proc_cpu",
                "pressure",
                "memory",
                "proc_memory",
                "network",
                "disk",
                "die_temps",
                "load",
                "swap",
                "iops",
                "disk_util",
            ],
            Self::Cpu => &[
                "cpu",
                "cpu_breakdown",
                "load",
                "cpu_mhz",
                "cpu_stall",
                "tasks",
                "proc_cpu",
            ],
            Self::Memory => &[
                "pressure",
                "memory",
                "memory_used",
                "swap",
                "memory_stall",
                "proc_memory",
            ],
            Self::Disk => &[
                "disk",
                "iops",
                "service_time",
                "disk_util",
                "root_space",
                "io_stall",
                "proc_read",
                "proc_write",
            ],
            Self::Network => &["network", "tcp_segments", "tcp_retransmits", "sockets"],
            Self::Thermals => &["die_temps", "ccd_temps", "storage_temps", "memory_temps", "fans"],
            Self::Gpu => &["gpu_util", "gpu_power", "gpu_memory", "die_temps"],
        }
    }

    /// The preset's charts that this machine can draw.
    pub fn resolve(self, known: &[&str]) -> Vec<&'static str> {
        self.charts()
            .iter()
            .copied()
            .filter(|id| chart(id).is_some_and(|c| c.available(known)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// Paths as this machine's daemon records them, including devices that must not be counted.
    const KNOWN: &[&str] = &[
        "/cpu/total_pct",
        "/cpu/temp_c",
        "/cpu/temps_c/Tccd1",
        "/cpu/temps_c/Tccd2",
        "/system/load/0",
        "/system/load/1",
        "/system/load/2",
        "/memory/anon_bytes",
        "/memory/slab_bytes",
        "/memory/kernel_stack_bytes",
        "/memory/page_tables_bytes",
        "/memory/cached_bytes",
        "/memory/zram/zram0/mem_used_bytes",
        "/memory/swap_used_bytes",
        "/memory/swap_in_pages_ps",
        "/pressure/memory/some/avg10",
        "/disk/nvme0n1/read_bps",
        "/disk/nvme1n1/read_bps",
        "/disk/zram0/read_bps",
        "/disk/nvme0n1/write_bps",
        "/disk/nvme0n1/busy_pct",
        "/disk/zram0/busy_pct",
        "/net/interfaces/enp11s0/rx_bps",
        "/net/interfaces/docker0/rx_bps",
        "/net/interfaces/enp11s0/tx_bps",
        "/gpu/devices/0000:01:00.0/temp_c",
        "/gpu/devices/0000:7a:00.0/temp_c",
        "/gpu/devices/0000:01:00.0/util_pct",
        "/hwmon/nvme~1nvme0/temp/Composite",
        "/hwmon/nvme~1nvme0/temp/Sensor 1",
        "/hwmon/nct6799~1nct6775.656/fan/fan1",
        "/hwmon/asusec~1asus-ec-sensors/fan/CPU_Opt",
        "/hwmon/nct6799~1nct6775.656/temp/SYSTIN",
    ];

    fn lines(id: &str) -> Vec<(String, Vec<String>, Combine)> {
        chart(id)
            .unwrap()
            .resolve(KNOWN)
            .into_iter()
            .map(|r| (r.label, r.source.paths, r.source.combine))
            .collect()
    }

    #[test]
    fn chart_ids_are_unique_and_every_preset_names_a_real_chart() {
        let mut seen = HashSet::new();
        assert!(CHARTS.iter().all(|c| seen.insert(c.id)), "duplicate chart id");
        for preset in Preset::ALL {
            for id in preset.charts() {
                assert!(chart(id).is_some(), "{preset:?} names unknown chart {id}");
            }
        }
        assert!(chart(HELPER_CHART).is_some());
        assert_eq!(
            Preset::Overview.charts().len(),
            12,
            "the overview fills four rows of three"
        );
    }

    #[test]
    fn memory_composition_combines_four_series_and_sums_the_kernel_parts() {
        let memory = lines("memory");
        let labels: Vec<&str> = memory.iter().map(|l| l.0.as_str()).collect();
        assert_eq!(labels, ["App", "Kernel", "Compressed", "Cached files"]);
        assert_eq!(memory[1].1.len(), 3, "slab, kernel stacks and page tables together");
        assert_eq!(memory[1].2, Combine::Sum);
        assert_eq!(memory[2].1, ["/memory/zram/zram0/mem_used_bytes"]);
    }

    #[test]
    fn load_averages_and_die_temperatures_are_one_chart_each() {
        let load = lines("load");
        assert_eq!(
            load.iter().map(|l| l.0.as_str()).collect::<Vec<_>>(),
            ["1 min", "5 min", "15 min"]
        );
        let die = lines("die_temps");
        assert_eq!(die[0].1, ["/cpu/temp_c"]);
        assert_eq!(die[1].1.len(), 2, "every GPU's die");
        assert_eq!(die[1].2, Combine::Max, "the hottest GPU, not the sum of temperatures");
    }

    #[test]
    fn totals_leave_out_devices_that_would_double_count() {
        let disk = lines("disk");
        assert_eq!(
            disk[0].1,
            ["/disk/nvme0n1/read_bps", "/disk/nvme1n1/read_bps"],
            "no zram"
        );
        assert_eq!(lines("disk_util")[0].1, ["/disk/nvme0n1/busy_pct"]);
        assert_eq!(lines("network")[0].1, ["/net/interfaces/enp11s0/rx_bps"], "no bridge");
    }

    #[test]
    fn pattern_charts_draw_one_named_line_per_device() {
        let ccd: Vec<String> = lines("ccd_temps").into_iter().map(|l| l.0).collect();
        assert_eq!(ccd, ["CCD 1", "CCD 2"]);
        let storage = lines("storage_temps");
        assert_eq!(storage.len(), 1, "the composite reading only");
        assert_eq!(storage[0].0, "nvme0");
        let fans: Vec<String> = lines("fans").into_iter().map(|l| l.0).collect();
        assert_eq!(fans, ["CPU_Opt", "fan1"]);
        assert_eq!(lines("gpu_util")[0].0, "GPU 01:00.0");
        let hues: Vec<Hue> = chart("ccd_temps")
            .unwrap()
            .resolve(KNOWN)
            .iter()
            .map(|r| r.hue)
            .collect();
        assert_ne!(hues[0], hues[1], "devices are told apart by colour");
    }

    #[test]
    fn pressure_needs_its_psi_series_and_charts_without_data_are_not_offered() {
        assert_eq!(lines("pressure")[0].2, Combine::Pressure);
        assert!(!chart("pressure").unwrap().available(&["/memory/swap_in_pages_ps"]));
        assert!(
            !chart("cpu_power").unwrap().available(KNOWN),
            "no package power without the helper"
        );
        assert!(
            chart("proc_cpu").unwrap().available(&[]),
            "process charts read their own table"
        );
        let overview = Preset::Overview.resolve(KNOWN);
        assert!(overview.contains(&"memory") && !overview.contains(&"iops"));
    }

    #[test]
    fn escaped_keys_read_as_written() {
        assert_eq!(unescape("~1run~1media"), "/run/media");
        assert_eq!(unescape("a~0b"), "a~b");
    }
}
