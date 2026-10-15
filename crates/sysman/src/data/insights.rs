use crate::charts::format::{bytes, duration};
use crate::data::history::Bucket;
use crate::data::pressure::{self, Band};

const KIB: f64 = 1024.;
const MIB: f64 = 1024. * KIB;
const GIB: f64 = 1024. * MIB;

/// A window's first point may start this much after the window does and still count as covering it.
const COVER_S: f64 = 60.;
/// A process whose newest stored minute is older than this has exited; minutes are stored once closed.
const ALIVE_S: f64 = 180.;

pub const SWAP_WINDOWS_S: [f64; 2] = [300., 900.];
/// The fast guard's window, and the stretch over which swap must still be growing.
pub const SWAP_RECENT_S: f64 = 120.;

pub const CPU_WINDOW_S: f64 = 1800.;
const CPU_MIN_MINUTES: usize = 20;
const CPU_AVG_PCT: f64 = 80.;
const CPU_BUSY_PCT: f64 = 25.;
const CPU_BUSY_SHARE: f64 = 0.75;

pub const GROWTH_WINDOW_S: f64 = 7200.;
const GROWTH_MIN_S: f64 = 1200.;
const GROWTH_SLOPE_BPS: f64 = 8. * KIB;
const GROWTH_FLOOR: f64 = 32. * MIB;
const GROWTH_RELATIVE: f64 = 0.05;
/// A single step up fits a line with R² of at most 0.75, so steady growth must fit better than that.
const GROWTH_FIT_R2: f64 = 0.8;

pub const PRESSURE_WINDOW_S: f64 = 7200.;
pub const OOM_WINDOW_S: f64 = 3600.;

pub const GPU_WINDOW_S: f64 = 300.;
const GPU_SUSTAIN_S: f64 = 30.;
const GPU_STALE_S: f64 = 30.;
const GPU_THERMAL: [&str; 3] = ["hw_thermal_slowdown", "sw_thermal_slowdown", "hw_slowdown"];
const GPU_POWER: [&str; 2] = ["sw_power_cap", "hw_power_brake_slowdown"];

pub const DISK_WINDOW_S: f64 = 3600.;
const DISK_MIN_S: f64 = 600.;
const DISK_STALE_S: f64 = 300.;
const DISK_FULL_PCT: f64 = 90.;
const DISK_CRITICAL_PCT: f64 = 97.;
/// Rise in percentage points over the window; below this the reading is noise.
const DISK_RISE_PCT: f64 = 0.05;
const DISK_ETA_MAX_S: f64 = 7. * 86400.;

/// Ordered most severe first, so sorting ranks callouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Critical,
    Warning,
    Advisory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Swap,
    Cpu,
    Growth,
    Pressure,
    Oom,
    GpuThrottle,
    DiskFull,
}

/// Where clicking the callout leads.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    None,
    Process { comm: String, pid: u32, start_time: u64 },
    Mount(String),
    Gpu,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Insight {
    pub kind: Kind,
    pub severity: Severity,
    pub title: String,
    pub value: String,
    pub detail: String,
    pub target: Target,
}

/// The most severe first, rule order breaking ties, at most `n`.
pub fn headline(mut all: Vec<Insight>, n: usize) -> Vec<Insight> {
    all.sort_by_key(|i| i.severity);
    all.truncate(n);
    all
}

fn value_at(points: &[(f64, f64)], start: f64) -> Option<f64> {
    let &(t, v) = points.iter().find(|p| p.0 >= start)?;
    (t <= start + COVER_S).then_some(v)
}

fn minutes(s: f64) -> String {
    format!("{:.0} min", s / 60.)
}

fn short_duration(s: f64) -> String {
    if s < 60. {
        format!("{s:.0} s")
    } else {
        duration(s as u64)
    }
}

/// Swap used `(t, bytes)` oldest first. Fires on growth past max(1 GiB, 3% RAM) over 5 or 15 min while swap is
/// still growing, or on max(2 GiB, 10% RAM) within 2 min.
pub fn swap(points: &[(f64, f64)], ram: f64, now: f64) -> Option<Insight> {
    let &(t_last, last) = points.last()?;
    if t_last < now - COVER_S * 2. {
        return None;
    }
    let floor = GIB.max(0.03 * ram);
    let fast = (2. * GIB).max(0.10 * ram);
    let growth = |w: f64| value_at(points, now - w).map(|v| (w, v, last - v));
    let recent = growth(SWAP_RECENT_S)?;
    let (window, from, grew, severity) = if recent.2 >= fast {
        (recent.0, recent.1, recent.2, Severity::Critical)
    } else if recent.2 > 0. {
        let (w, v, g) = SWAP_WINDOWS_S
            .iter()
            .filter_map(|&w| growth(w))
            .filter(|g| g.2 >= floor)
            .max_by(|a, b| a.2.total_cmp(&b.2))?;
        (w, v, g, Severity::Warning)
    } else {
        return None;
    };
    Some(Insight {
        kind: Kind::Swap,
        severity,
        title: "Swap is climbing".into(),
        value: format!("+{}", bytes(grew)),
        detail: format!(
            "Swap grew {} over the last {}, from {} to {}. A sustained climb means memory is oversubscribed.",
            bytes(grew),
            minutes(window),
            bytes(from),
            bytes(last)
        ),
        target: Target::None,
    })
}

/// One stored minute of one process.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Minute {
    pub t: f64,
    pub rss: Option<f64>,
    pub cpu: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProcHistory {
    pub comm: String,
    pub cmdline: Option<String>,
    pub pid: u32,
    pub start_time: u64,
    /// Oldest first.
    pub minutes: Vec<Minute>,
}

impl ProcHistory {
    fn alive(&self, now: f64) -> bool {
        self.minutes.last().is_some_and(|m| m.t >= now - ALIVE_S)
    }

    fn target(&self) -> Target {
        Target::Process {
            comm: self.comm.clone(),
            pid: self.pid,
            start_time: self.start_time,
        }
    }
}

/// A running process averaging at least 80% of a core over the last 30 min, busy (25%+) in three quarters of the
/// minutes, with at least 20 minutes stored.
pub fn busy_cpu(procs: &[ProcHistory], now: f64) -> Option<Insight> {
    let (p, mean) = procs
        .iter()
        .filter(|p| p.alive(now))
        .filter_map(|p| {
            let cpu: Vec<f64> = p
                .minutes
                .iter()
                .filter(|m| m.t >= now - CPU_WINDOW_S)
                .filter_map(|m| m.cpu)
                .collect();
            if cpu.len() < CPU_MIN_MINUTES {
                return None;
            }
            let n = cpu.len() as f64;
            let mean = cpu.iter().sum::<f64>() / n;
            let busy = cpu.iter().filter(|&&c| c >= CPU_BUSY_PCT).count() as f64 / n;
            (mean >= CPU_AVG_PCT && busy >= CPU_BUSY_SHARE).then_some((p, mean))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    Some(Insight {
        kind: Kind::Cpu,
        severity: Severity::Advisory,
        title: format!("{} is using a lot of CPU", p.comm),
        value: format!("{mean:.0}%"),
        detail: format!(
            "It has averaged {mean:.0}% of one core over the last 30 min. If it is not doing work you expect, \
             restarting it usually settles it."
        ),
        target: p.target(),
    })
}

/// A process whose memory grew steadily.
#[derive(Debug, Clone, PartialEq)]
pub struct Growth {
    pub comm: String,
    pub cmdline: Option<String>,
    pub pid: u32,
    pub start_time: u64,
    pub slope_bps: f64,
    /// Along the fitted line, so one noisy end minute does not decide it.
    pub grew: f64,
    pub now_bytes: f64,
    pub observed_s: f64,
    /// `(t, rss)` oldest first.
    pub points: Vec<(f64, f64)>,
}

impl Growth {
    pub fn target(&self) -> Target {
        Target::Process {
            comm: self.comm.clone(),
            pid: self.pid,
            start_time: self.start_time,
        }
    }
}

/// Least-squares slope, intercept at the first point, and R².
fn fit(points: &[(f64, f64)]) -> Option<(f64, f64, f64)> {
    let n = points.len() as f64;
    if n < 3. {
        return None;
    }
    let t0 = points[0].0;
    let mx = points.iter().map(|p| p.0 - t0).sum::<f64>() / n;
    let my = points.iter().map(|p| p.1).sum::<f64>() / n;
    let (mut sxx, mut sxy, mut syy) = (0., 0., 0.);
    for &(t, y) in points {
        let (dx, dy) = (t - t0 - mx, y - my);
        sxx += dx * dx;
        sxy += dx * dy;
        syy += dy * dy;
    }
    if sxx <= 0. {
        return None;
    }
    let slope = sxy / sxx;
    let r2 = if syy > 0. { sxy * sxy / (sxx * syy) } else { 0. };
    Some((slope, my - slope * mx, r2))
}

/// Running processes whose memory rose along a straight line over at least 20 of the last 120 min: 8 KiB/s,
/// 32 MiB and 5% at least. Largest growth first.
pub fn growth(procs: &[ProcHistory], now: f64) -> Vec<Growth> {
    let mut out: Vec<Growth> = procs
        .iter()
        .filter(|p| p.alive(now))
        .filter_map(|p| {
            let points: Vec<(f64, f64)> = p
                .minutes
                .iter()
                .filter(|m| m.t >= now - GROWTH_WINDOW_S)
                .filter_map(|m| Some((m.t, m.rss?)))
                .collect();
            let observed_s = points.last()?.0 - points.first()?.0;
            if observed_s < GROWTH_MIN_S {
                return None;
            }
            let (slope, start, r2) = fit(&points)?;
            let grew = slope * observed_s;
            let steady = slope >= GROWTH_SLOPE_BPS
                && grew >= GROWTH_FLOOR
                && start > 0.
                && grew / start >= GROWTH_RELATIVE
                && r2 >= GROWTH_FIT_R2;
            steady.then(|| Growth {
                comm: p.comm.clone(),
                cmdline: p.cmdline.clone(),
                pid: p.pid,
                start_time: p.start_time,
                slope_bps: slope,
                grew,
                now_bytes: points[points.len() - 1].1,
                observed_s,
                points,
            })
        })
        .collect();
    out.sort_by(|a, b| b.grew.total_cmp(&a.grew));
    out
}

/// The largest steady growth, once it passes max(512 MiB, 3% RAM); smaller ones stay on the growth board.
pub fn growth_insight(board: &[Growth], ram: f64) -> Option<Insight> {
    let warning = (512. * MIB).max(0.03 * ram);
    let g = board.iter().find(|g| g.grew >= warning)?;
    Some(Insight {
        kind: Kind::Growth,
        severity: Severity::Warning,
        title: format!("{} keeps growing", g.comm),
        value: format!("+{}", bytes(g.grew)),
        detail: format!(
            "Its memory rose steadily by {} over {}, about {}/min, and is now {}. Growth alone does not prove a \
             leak; restarting it gives the memory back.",
            bytes(g.grew),
            minutes(g.observed_s),
            bytes(g.slope_bps * 60.),
            bytes(g.now_bytes)
        ),
        target: g.target(),
    })
}

/// A spell of memory pressure at Warning or above, marked at its peak.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Event {
    pub t: f64,
    pub start: f64,
    pub end: f64,
    pub peak: f64,
    pub band: Band,
}

/// Runs of pressure-index buckets whose peak reaches Warning; a missing bucket ends a run.
pub fn pressure_events(buckets: &[Bucket], width_s: f64) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::new();
    let mut last_t = f64::NEG_INFINITY;
    let mut open = false;
    for b in buckets {
        let t = b.t as f64;
        let contiguous = t - last_t <= width_s * 1.5;
        last_t = t;
        if b.max < pressure::WARNING {
            open = false;
            continue;
        }
        match out.last_mut() {
            Some(e) if open && contiguous => {
                e.end = t + width_s;
                if b.max > e.peak {
                    (e.peak, e.t) = (b.max, t + width_s / 2.);
                }
            }
            _ => out.push(Event {
                t: t + width_s / 2.,
                start: t,
                end: t + width_s,
                peak: b.max,
                band: Band::Warning,
            }),
        }
        open = true;
    }
    for e in &mut out {
        e.band = pressure::band(e.peak);
    }
    out
}

pub fn band_label(band: Band) -> &'static str {
    match band {
        Band::Normal => "Normal",
        Band::Warning => "Warning",
        Band::Critical => "Critical",
    }
}

fn times(n: usize) -> String {
    match n {
        1 => "once".into(),
        2 => "twice".into(),
        n => format!("{n} times"),
    }
}

/// Any pressure event in the window; Critical when one reached Critical.
pub fn pressure_insight(events: &[Event], now: f64) -> Option<Insight> {
    let latest = events.last()?;
    let worst = events.iter().map(|e| e.band).max_by_key(|b| *b == Band::Critical)?;
    let peak = events.iter().map(|e| e.peak).fold(0., f64::max);
    let ago = (now - latest.end).max(0.);
    let when = if ago < 60. {
        "just now".into()
    } else {
        format!("{} ago", short_duration(ago))
    };
    Some(Insight {
        kind: Kind::Pressure,
        severity: if worst == Band::Critical {
            Severity::Critical
        } else {
            Severity::Warning
        },
        title: "Memory pressure spiked".into(),
        value: format!("{peak:.0}"),
        detail: format!(
            "Pressure reached {} {} in the last 2 hours, most recently {when}. Programs stalled waiting for memory.",
            band_label(worst),
            times(events.len())
        ),
        target: Target::None,
    })
}

/// Kills counted by `oom_kills_total` `(t, total)` over the last hour; a falling counter means a reboot.
pub fn oom(points: &[(f64, f64)], now: f64) -> Option<Insight> {
    let start = now - OOM_WINDOW_S;
    let mut kills = 0.;
    // The last reading before the window is the baseline, so a kill just after it begins still counts.
    let mut prev = points.iter().rev().find(|p| p.0 < start).map(|p| p.1);
    for &(_, v) in points.iter().filter(|p| p.0 >= start) {
        if let Some(p) = prev {
            kills += if v >= p { v - p } else { v };
        }
        prev = Some(v);
    }
    let kills = kills.round();
    if kills < 1. {
        return None;
    }
    Some(Insight {
        kind: Kind::Oom,
        severity: Severity::Critical,
        title: if kills == 1. {
            "The kernel killed a process for memory".into()
        } else {
            format!("The kernel killed {kills:.0} processes for memory")
        },
        value: format!("+{kills:.0}"),
        detail: "The out-of-memory killer ran in the last hour: something needed more memory than the system had. \
                 The kernel log names what it killed."
            .into(),
        target: Target::None,
    })
}

/// One GPU's `(t, throttle reasons)`, oldest first.
pub type Readings = Vec<(f64, Vec<String>)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Throttle {
    Thermal,
    Power,
}

/// An idle GPU reports its power cap as a reason too, which says nothing about load.
fn throttle(reasons: &[String]) -> Option<Throttle> {
    let has = |names: &[&str]| reasons.iter().any(|r| names.contains(&r.as_str()));
    if has(&["gpu_idle"]) {
        None
    } else if has(&GPU_THERMAL) {
        Some(Throttle::Thermal)
    } else if has(&GPU_POWER) {
        Some(Throttle::Power)
    } else {
        None
    }
}

/// Per GPU, its name and `(t, throttle reasons)` oldest first. Fires when the newest readings have been throttled
/// for 30 s without a break; thermal outranks power.
pub fn gpu_throttle(devices: &[(String, Readings)], now: f64) -> Option<Insight> {
    let (name, kind, held) = devices
        .iter()
        .filter_map(|(name, readings)| {
            let (t_last, _) = readings.last()?;
            if *t_last < now - GPU_STALE_S {
                return None;
            }
            let run: Vec<(f64, Throttle)> = readings
                .iter()
                .rev()
                .map_while(|(t, reasons)| Some((*t, throttle(reasons)?)))
                .collect();
            let held = t_last - run.last()?.0;
            let kind = if run.iter().any(|r| r.1 == Throttle::Thermal) {
                Throttle::Thermal
            } else {
                Throttle::Power
            };
            (held >= GPU_SUSTAIN_S).then_some((name, kind, held))
        })
        .min_by_key(|(_, kind, _)| *kind != Throttle::Thermal)?;
    let held_text = short_duration(held);
    let (severity, title, detail) = match kind {
        Throttle::Thermal => (
            Severity::Warning,
            format!("{name} is thermal throttling"),
            format!(
                "Its clocks have been cut for {held_text} to stay within its temperature limit. Check airflow, dust \
                 and the fan curve."
            ),
        ),
        Throttle::Power => (
            Severity::Advisory,
            format!("{name} is held at its power limit"),
            format!(
                "Its clocks have been capped by the board power limit for {held_text}, so it runs slower than the \
                 load asks for."
            ),
        ),
    };
    Some(Insight {
        kind: Kind::GpuThrottle,
        severity,
        title,
        value: held_text,
        detail,
        target: Target::Gpu,
    })
}

/// Per mount, `(t, used %)` oldest first. Fires when one is above 90% and rose over the last hour.
pub fn disk_full(mounts: &[(String, Vec<(f64, f64)>)], now: f64) -> Option<Insight> {
    let (mount, last, rise, span) = mounts
        .iter()
        .filter_map(|(mount, points)| {
            let window: Vec<&(f64, f64)> = points.iter().filter(|p| p.0 >= now - DISK_WINDOW_S).collect();
            let (&&(t0, first), &&(t1, last)) = (window.first()?, window.last()?);
            let (rise, span) = (last - first, t1 - t0);
            let filling =
                t1 >= now - DISK_STALE_S && span >= DISK_MIN_S && last > DISK_FULL_PCT && rise >= DISK_RISE_PCT;
            filling.then_some((mount, last, rise, span))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    let eta = (100. - last) / (rise / span);
    let eta = if eta <= DISK_ETA_MAX_S {
        format!(" At this rate it is full in about {}.", short_duration(eta))
    } else {
        String::new()
    };
    Some(Insight {
        kind: Kind::DiskFull,
        severity: if last >= DISK_CRITICAL_PCT {
            Severity::Critical
        } else {
            Severity::Warning
        },
        title: format!("{mount} is filling up"),
        value: format!("{last:.0}%"),
        detail: format!(
            "It is {last:.0}% full and rose {rise:.1} points over the last {}.{eta}",
            minutes(span)
        ),
        target: Target::Mount(mount.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_000_000.;
    const RAM: f64 = 16. * GIB;

    /// `(t, v)` every `step` seconds from `NOW - span` to `NOW`, valued by `f(seconds before now)`.
    fn series(span: f64, step: f64, f: impl Fn(f64) -> f64) -> Vec<(f64, f64)> {
        let n = (span / step) as usize;
        (0..=n)
            .map(|i| {
                let ago = span - i as f64 * step;
                (NOW - ago, f(ago))
            })
            .collect()
    }

    mod swap {
        use super::*;

        /// Swap rising linearly by `total` over the last `over` seconds, flat before.
        fn rising(total: f64, over: f64) -> Vec<(f64, f64)> {
            series(1200., 10., |ago| 4. * GIB + total * (1. - ago.min(over) / over))
        }

        #[test]
        fn a_sustained_climb_past_the_floor_fires_with_its_growth() {
            let i = swap(&rising(1.5 * GIB, 900.), RAM, NOW).expect("1.5 GiB over 15 min passes the 1 GiB floor");
            assert_eq!(i.severity, Severity::Warning);
            assert_eq!(i.value, "+1.5 GB");
            assert!(
                i.detail.contains("15 min") && i.detail.contains("from 4.0 GB to 5.5 GB"),
                "{}",
                i.detail
            );
        }

        #[test]
        fn growth_below_the_floor_does_not_fire() {
            assert!(swap(&rising(0.9 * GIB, 900.), RAM, NOW).is_none());
        }

        #[test]
        fn the_floor_scales_with_ram() {
            let big = 64. * GIB;
            assert!(
                swap(&rising(1.5 * GIB, 900.), big, NOW).is_none(),
                "3% of 64 GiB is 1.9 GiB, above the growth"
            );
            assert!(swap(&rising(2.0 * GIB, 900.), big, NOW).is_some());
        }

        #[test]
        fn growth_exactly_at_the_floor_fires() {
            assert!(swap(&rising(GIB, 900.), RAM, NOW).is_some());
            assert!(swap(&rising(GIB - MIB, 900.), RAM, NOW).is_none());
        }

        #[test]
        fn a_climb_that_has_stopped_does_not_fire() {
            let stopped = series(1200., 10., |ago| {
                if ago <= 180. {
                    6. * GIB
                } else {
                    4. * GIB + 2. * GIB * (1. - ago.min(900.) / 900.)
                }
            });
            assert!(swap(&stopped, RAM, NOW).is_none(), "flat over the last 2 min");
        }

        #[test]
        fn a_burst_past_the_fast_guard_fires_as_critical() {
            let burst = rising(2.5 * GIB, 100.);
            let i = swap(&burst, RAM, NOW).expect("2.5 GiB in 2 min passes the 2 GiB guard");
            assert_eq!(i.severity, Severity::Critical);
            assert!(i.detail.contains("2 min"), "{}", i.detail);
        }

        #[test]
        fn an_uncovered_window_or_stale_data_does_not_fire() {
            let short: Vec<_> = rising(3. * GIB, 900.)
                .into_iter()
                .filter(|p| p.0 >= NOW - 30.)
                .collect();
            assert!(
                swap(&short, RAM, NOW).is_none(),
                "no point near the start of the 2 min window"
            );
            let stale: Vec<_> = rising(1.5 * GIB, 900.)
                .into_iter()
                .map(|(t, v)| (t - 600., v))
                .collect();
            assert!(swap(&stale, RAM, NOW).is_none());
            assert!(swap(&[], RAM, NOW).is_none());
        }
    }

    fn proc_with(comm: &str, minutes: Vec<Minute>) -> ProcHistory {
        ProcHistory {
            comm: comm.into(),
            cmdline: None,
            pid: 42,
            start_time: 7,
            minutes,
        }
    }

    /// One minute per minute over the last `span` s, newest one minute ago as the daemon stores them.
    fn per_minute(span: f64, rss: impl Fn(f64) -> Option<f64>, cpu: impl Fn(f64) -> Option<f64>) -> Vec<Minute> {
        series(span, 60., |ago| ago)
            .into_iter()
            .map(|(t, ago)| Minute {
                t: t - 60.,
                rss: rss(ago),
                cpu: cpu(ago),
            })
            .collect()
    }

    mod cpu {
        use super::*;

        /// `stored` minutes, the newest one minute ago.
        fn busy(avg: f64, stored: f64) -> ProcHistory {
            proc_with("encoder", per_minute((stored - 1.) * 60., |_| Some(1e8), |_| Some(avg)))
        }

        #[test]
        fn a_sustained_busy_process_fires_with_its_average() {
            let i = busy_cpu(&[busy(90., 40.)], NOW).expect("90% for 40 min");
            assert_eq!(i.title, "encoder is using a lot of CPU");
            assert_eq!(i.value, "90%");
            assert_eq!(
                i.target,
                Target::Process {
                    comm: "encoder".into(),
                    pid: 42,
                    start_time: 7
                }
            );
        }

        #[test]
        fn the_average_threshold_is_inclusive() {
            assert!(busy_cpu(&[busy(80., 40.)], NOW).is_some());
            assert!(busy_cpu(&[busy(79.9, 40.)], NOW).is_none());
        }

        #[test]
        fn too_little_history_does_not_fire() {
            assert!(
                busy_cpu(&[busy(150., 19.)], NOW).is_none(),
                "19 minutes is under the 20 needed"
            );
            assert!(busy_cpu(&[busy(150., 20.)], NOW).is_some());
        }

        #[test]
        fn a_short_burst_with_a_high_average_does_not_fire() {
            // 400% for 7 of 30 minutes averages over 80%, but the process is idle most of the time.
            let bursty = proc_with(
                "build",
                per_minute(1800., |_| Some(1e8), |ago| Some(if ago < 420. { 400. } else { 1. })),
            );
            assert!(busy_cpu(&[bursty], NOW).is_none());
        }

        #[test]
        fn an_exited_process_does_not_fire() {
            let mut p = busy(95., 40.);
            for m in &mut p.minutes {
                m.t -= 600.;
            }
            assert!(busy_cpu(&[p], NOW).is_none());
        }

        #[test]
        fn the_busiest_process_wins() {
            let mut other = busy(120., 40.);
            other.comm = "game".into();
            assert_eq!(busy_cpu(&[busy(90., 40.), other], NOW).unwrap().value, "120%");
        }
    }

    mod growth {
        use super::*;

        /// RSS from `base`, growing `per_min` each minute over `span` s.
        fn growing(base: f64, per_min: f64, span: f64) -> ProcHistory {
            proc_with(
                "leaky",
                per_minute(span, |ago| Some(base + per_min * (span - ago) / 60.), |_| Some(1.)),
            )
        }

        #[test]
        fn steady_growth_past_every_floor_is_on_the_board() {
            let board = growth(&[growing(400. * MIB, 2. * MIB, 3600.)], NOW);
            assert_eq!(board.len(), 1);
            let g = &board[0];
            assert!((g.slope_bps - 2. * MIB / 60.).abs() < 1., "{}", g.slope_bps);
            assert!((g.grew - 120. * MIB).abs() < MIB, "{}", g.grew / MIB);
            assert!(g.points.len() > 50);
        }

        #[test]
        fn twenty_minutes_is_the_least_observed() {
            // 3 MiB/min for 19 min is 57 MiB, past the floors, but too short to call steady.
            assert!(growth(&[growing(100. * MIB, 3. * MIB, 19. * 60.)], NOW).is_empty());
            assert_eq!(growth(&[growing(100. * MIB, 3. * MIB, 20. * 60.)], NOW).len(), 1);
        }

        #[test]
        fn each_floor_holds_on_its_own() {
            // Slope: 0.4 MiB/min is under 8 KiB/s even though 2 h of it passes 32 MiB.
            assert!(growth(&[growing(100. * MIB, 0.4 * MIB, 7200.)], NOW).is_empty());
            // Size: 1 MiB/min for 25 min is 25 MiB, under 32 MiB.
            assert!(growth(&[growing(100. * MIB, MIB, 1500.)], NOW).is_empty());
            // Relative: 40 MiB on 2 GiB is 2%.
            assert!(growth(&[growing(2048. * MIB, 1. * MIB, 2400.)], NOW).is_empty());
        }

        #[test]
        fn a_single_step_up_is_not_steady_growth() {
            let step = proc_with(
                "step",
                per_minute(
                    3600.,
                    |ago| Some(if ago < 1800. { 600. * MIB } else { 200. * MIB }),
                    |_| None,
                ),
            );
            assert!(growth(&[step], NOW).is_empty());
        }

        #[test]
        fn shrinking_memory_and_exited_processes_are_not_listed() {
            assert!(growth(&[growing(900. * MIB, -3. * MIB, 3600.)], NOW).is_empty());
            let mut gone = growing(100. * MIB, 3. * MIB, 3600.);
            for m in &mut gone.minutes {
                m.t -= 900.;
            }
            assert!(growth(&[gone], NOW).is_empty());
        }

        #[test]
        fn only_growth_past_the_warning_size_becomes_a_callout() {
            let small = growth(&[growing(400. * MIB, 2. * MIB, 3600.)], NOW);
            assert!(growth_insight(&small, RAM).is_none(), "120 MiB is under 512 MiB");
            let large = growth(&[growing(1024. * MIB, 10. * MIB, 3600.)], NOW);
            let i = growth_insight(&large, RAM).expect("600 MiB passes 512 MiB");
            assert_eq!((i.kind, i.severity), (Kind::Growth, Severity::Warning));
            assert_eq!(i.title, "leaky keeps growing");
            assert!(
                growth_insight(&large, 64. * GIB).is_none(),
                "3% of 64 GiB is 1.9 GiB: the warning size scales with RAM"
            );
        }
    }

    mod pressure_rules {
        use super::*;

        fn buckets(values: &[f64]) -> Vec<Bucket> {
            values
                .iter()
                .enumerate()
                .map(|(i, &v)| Bucket {
                    t: 1000 + i as i64 * 20,
                    min: v,
                    avg: v,
                    max: v,
                })
                .collect()
        }

        #[test]
        fn each_run_at_warning_or_above_is_one_event_marked_at_its_peak() {
            let events = pressure_events(&buckets(&[5., 40., 70., 50., 10., 33.9, 34., 20.]), 20.);
            assert_eq!(events.len(), 2, "33.9 is still Normal; 34 starts Warning");
            assert_eq!((events[0].peak, events[0].band), (70., Band::Critical));
            assert_eq!((events[0].start, events[0].end, events[0].t), (1020., 1080., 1050.));
            assert_eq!((events[1].peak, events[1].band), (34., Band::Warning));
        }

        #[test]
        fn a_gap_in_the_data_splits_a_run() {
            let mut b = buckets(&[50., 50.]);
            b[1].t += 100;
            assert_eq!(pressure_events(&b, 20.).len(), 2);
        }

        #[test]
        fn calm_history_has_no_events_and_no_callout() {
            let events = pressure_events(&buckets(&[0., 10., 33.]), 20.);
            assert!(events.is_empty());
            assert!(pressure_insight(&events, NOW).is_none());
        }

        #[test]
        fn the_callout_takes_the_worst_band_and_counts_events() {
            let events = pressure_events(&buckets(&[40., 10., 80., 10., 45.]), 20.);
            let i = pressure_insight(&events, 1100. + 300.).unwrap();
            assert_eq!((i.severity, i.value.as_str()), (Severity::Critical, "80"));
            assert!(
                i.detail.contains("Critical 3 times") && i.detail.contains("5m ago"),
                "{}",
                i.detail
            );
            let mild = pressure_events(&buckets(&[40.]), 20.);
            let i = pressure_insight(&mild, 1020.).unwrap();
            assert_eq!(i.severity, Severity::Warning);
            assert!(
                i.detail.contains("Warning once") && i.detail.contains("just now"),
                "{}",
                i.detail
            );
        }
    }

    mod oom {
        use super::*;

        #[test]
        fn a_rising_counter_fires_with_the_kills_in_the_last_hour() {
            let points = series(7200., 60., |ago| {
                if ago > 3600. {
                    0.
                } else if ago > 600. {
                    3.
                } else {
                    5.
                }
            });
            let i = oom(&points, NOW).unwrap();
            assert_eq!((i.severity, i.value.as_str()), (Severity::Critical, "+5"));
            assert_eq!(i.title, "The kernel killed 5 processes for memory");
        }

        #[test]
        fn a_flat_counter_or_kills_before_the_window_do_not_fire() {
            assert!(
                oom(&series(3600., 60., |_| 12.), NOW).is_none(),
                "old kills are not news"
            );
            let earlier = series(7200., 60., |ago| if ago > 3700. { 0. } else { 2. });
            assert!(oom(&earlier, NOW).is_none(), "the rise happened before the hour began");
        }

        #[test]
        fn a_reboot_resets_the_counter_without_hiding_new_kills() {
            let points = [(NOW - 600., 9.), (NOW - 300., 0.), (NOW - 200., 1.)];
            let i = oom(&points, NOW).unwrap();
            assert_eq!(
                (i.value.as_str(), i.title.as_str()),
                ("+1", "The kernel killed a process for memory")
            );
        }
    }

    mod gpu {
        use super::*;

        fn readings(reasons: &[&str], from_ago: f64) -> Vec<(f64, Vec<String>)> {
            series(from_ago, 2., |_| 0.)
                .into_iter()
                .map(|(t, _)| (t, reasons.iter().map(|s| s.to_string()).collect()))
                .collect()
        }

        fn device(r: Readings) -> Vec<(String, Readings)> {
            vec![("RTX 4070".into(), r)]
        }

        #[test]
        fn sustained_thermal_throttling_fires_as_a_warning() {
            let i = gpu_throttle(&device(readings(&["sw_thermal_slowdown"], 60.)), NOW).unwrap();
            assert_eq!((i.severity, i.value.as_str()), (Severity::Warning, "1m"));
            assert_eq!(i.title, "RTX 4070 is thermal throttling");
        }

        #[test]
        fn a_power_cap_under_load_is_advisory_but_an_idle_one_is_nothing() {
            let i = gpu_throttle(&device(readings(&["sw_power_cap"], 40.)), NOW).unwrap();
            assert_eq!((i.severity, i.value.as_str()), (Severity::Advisory, "40 s"));
            assert!(gpu_throttle(&device(readings(&["gpu_idle", "sw_power_cap"], 120.)), NOW).is_none());
        }

        #[test]
        fn thirty_seconds_without_a_break_is_needed() {
            assert!(gpu_throttle(&device(readings(&["hw_slowdown"], 28.)), NOW).is_none());
            assert!(gpu_throttle(&device(readings(&["hw_slowdown"], 30.)), NOW).is_some());
            let mut broken = readings(&["hw_slowdown"], 120.);
            let n = broken.len();
            broken[n - 15].1.clear();
            assert!(
                gpu_throttle(&device(broken), NOW).is_none(),
                "a clear reading 28 s ago restarts the run"
            );
        }

        #[test]
        fn a_throttle_that_ended_or_stale_readings_do_not_fire() {
            let mut ended = readings(&["sw_thermal_slowdown"], 120.);
            ended.last_mut().unwrap().1.clear();
            assert!(gpu_throttle(&device(ended), NOW).is_none());
            let stale: Vec<_> = readings(&["sw_thermal_slowdown"], 120.)
                .into_iter()
                .map(|(t, r)| (t - 60., r))
                .collect();
            assert!(gpu_throttle(&device(stale), NOW).is_none());
        }
    }

    mod disk {
        use super::*;

        fn mount(points: Vec<(f64, f64)>) -> Vec<(String, Vec<(f64, f64)>)> {
            vec![("/home".into(), points)]
        }

        #[test]
        fn a_full_and_rising_filesystem_fires_with_an_eta() {
            let i = disk_full(&mount(series(3600., 60., |ago| 94. - ago / 3600.)), NOW).unwrap();
            assert_eq!((i.severity, i.value.as_str()), (Severity::Warning, "94%"));
            assert_eq!(i.target, Target::Mount("/home".into()));
            assert!(
                i.detail.contains("rose 1.0 points") && i.detail.contains("full in about 6h 0m"),
                "{}",
                i.detail
            );
        }

        #[test]
        fn ninety_percent_is_not_yet_full() {
            assert!(disk_full(&mount(series(3600., 60., |ago| 90. - ago / 3600.)), NOW).is_none());
            assert!(disk_full(&mount(series(3600., 60., |ago| 90.1 - ago / 3600.)), NOW).is_some());
        }

        #[test]
        fn a_full_but_steady_or_shrinking_filesystem_does_not_fire() {
            assert!(
                disk_full(&mount(series(3600., 60., |_| 99.)), NOW).is_none(),
                "full but not rising"
            );
            assert!(disk_full(&mount(series(3600., 60., |ago| 95. + ago / 3600.)), NOW).is_none());
            assert!(
                disk_full(&mount(series(3600., 60., |ago| 95. - ago / 3600. * 0.04)), NOW).is_none(),
                "noise"
            );
        }

        #[test]
        fn nearly_full_is_critical_and_slow_fills_carry_no_eta() {
            let i = disk_full(&mount(series(3600., 60., |ago| 98. - ago / 3600. * 0.1)), NOW).unwrap();
            assert_eq!(i.severity, Severity::Critical);
            assert!(i.detail.contains("full in about 20h 0m"), "{}", i.detail);
            let slow = disk_full(&mount(series(3600., 60., |ago| 90.1 - ago / 3600. * 0.051)), NOW).unwrap();
            assert_eq!(slow.severity, Severity::Warning);
            assert!(
                !slow.detail.contains("At this rate"),
                "over a week away: {}",
                slow.detail
            );
        }

        #[test]
        fn too_short_or_stale_history_does_not_fire() {
            assert!(disk_full(&mount(series(300., 60., |ago| 95. - ago / 60.)), NOW).is_none());
            let stale = series(3600., 60., |ago| 95. - ago / 3600.)
                .into_iter()
                .map(|(t, v)| (t - 900., v))
                .collect();
            assert!(disk_full(&mount(stale), NOW).is_none());
        }
    }

    #[test]
    fn headline_ranks_by_severity_and_keeps_rule_order_for_ties() {
        let mk = |kind, severity| Insight {
            kind,
            severity,
            title: String::new(),
            value: String::new(),
            detail: String::new(),
            target: Target::None,
        };
        let top = headline(
            vec![
                mk(Kind::Cpu, Severity::Advisory),
                mk(Kind::Swap, Severity::Warning),
                mk(Kind::Oom, Severity::Critical),
                mk(Kind::DiskFull, Severity::Warning),
            ],
            2,
        );
        assert_eq!(top.iter().map(|i| i.kind).collect::<Vec<_>>(), [Kind::Oom, Kind::Swap]);
    }
}
