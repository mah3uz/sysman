//! Why a running process exists: its cause from the cgroup and parent chain (`classify`, pure), refined with unit,
//! session, desktop-file and container details (`explain`, blocking, best effort).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use schema::cgroups::cgroup_path;
use schema::processes::Process;

use super::desktop::data_dirs;
use super::history::{History, Origin, Proc};
use crate::screens::logs::actions::restartable_unit;

const TIMEOUT: Duration = Duration::from_secs(1);
const MAX_CHAIN: usize = 64;
const DOCKER_SOCKET: &str = "/var/run/docker.sock";

const TERMINALS: &[&str] = &[
    "kitty",
    "alacritty",
    "foot",
    "footclient",
    "wezterm-gui",
    "gnome-terminal-server",
    "konsole",
    "xterm",
    "urxvt",
    "terminator",
    "tilix",
    "xfce4-terminal",
    "lxterminal",
    "ghostty",
    "ptyxis-agent",
    "sakura",
    "qterminal",
    "mate-terminal",
    "yakuake",
    "tmux",
    "screen",
];
const SHELLS: &[&str] = &[
    "bash", "zsh", "fish", "sh", "dash", "ksh", "tcsh", "csh", "nu", "elvish", "xonsh",
];
const CRON: &[&str] = &["cron", "crond", "anacron", "fcron"];

/// One process in the chain from the selected process up to PID 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub pid: u32,
    pub comm: String,
    pub exe: Option<String>,
}

/// A systemd unit as the cgroup names it (`\x2d` escapes kept, as systemctl and D-Bus take them). `uid` is set for a
/// unit run by that user's manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub name: String,
    pub uid: Option<u32>,
}

impl Unit {
    pub fn display(&self) -> String {
        unescape(&self.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    Docker,
    Podman,
    Kubernetes,
    Machine,
}

impl Runtime {
    fn label(self) -> &'static str {
        match self {
            Runtime::Docker => "Docker container",
            Runtime::Podman => "Podman container",
            Runtime::Kubernetes => "Kubernetes container",
            Runtime::Machine => "machine",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    Init,
    Kernel,
    Service(Unit),
    Socket {
        unit: Unit,
        socket: String,
    },
    Timer {
        unit: Unit,
        timer: String,
    },
    DBus {
        unit: Unit,
        bus_name: String,
    },
    /// `desktop_id` is the unit name's `app-…` part, raw; a launcher prefix may still be on it.
    Autostart {
        unit: Unit,
        desktop_id: String,
    },
    App {
        unit: Unit,
        desktop_id: String,
    },
    /// Comm names from the outermost terminal or shell down to the process.
    Terminal {
        path: Vec<String>,
        emulator: bool,
    },
    Orphan,
    Session {
        id: String,
    },
    Container {
        runtime: Runtime,
        id: Option<String>,
    },
    Flatpak {
        app: String,
    },
    Snap {
        name: String,
    },
    Cron,
    Unknown {
        reason: String,
    },
}

pub fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'\\' && b.get(i + 1) == Some(&b'x'))
            .then(|| s.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(h, 16).ok()))
            .flatten();
        match hex {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_hex_id(s: &str) -> bool {
    s.len() >= 12 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn scope_id(comp: &str, prefixes: &[&str]) -> Option<String> {
    let rest = comp.strip_suffix(".scope")?;
    prefixes
        .iter()
        .find_map(|p| rest.strip_prefix(p))
        .filter(|id| is_hex_id(id))
        .map(str::to_owned)
}

/// The `app-` unit's `[<launcher>-]<ApplicationID>` part with its random suffix removed, still escaped. systemd names
/// scopes `app-[<launcher>-]<ID>-<RANDOM>.scope` and services `app-[<launcher>-]<ID>@<RANDOM>.service`.
fn app_desktop_id(unit: &str) -> String {
    let stem = unit.strip_prefix("app-").unwrap_or(unit);
    let scope = stem.ends_with(".scope");
    let stem = stem
        .strip_suffix(".service")
        .or_else(|| stem.strip_suffix(".scope"))
        .unwrap_or(stem);
    if let Some((id, _)) = stem.split_once('@') {
        return id.to_owned();
    }
    match stem.rsplit_once('-') {
        Some((id, random)) if scope && !id.is_empty() && !random.is_empty() => id.to_owned(),
        _ => stem.to_owned(),
    }
}

/// Desktop ids to try for an app unit's raw id: as is, and without a leading launcher segment. A real id's own dashes
/// are escaped, so a bare dash separates the launcher.
fn desktop_candidates(raw: &str) -> Vec<String> {
    let mut out = vec![unescape(raw)];
    if let Some((_, rest)) = raw.split_once('-') {
        out.push(unescape(rest));
    }
    out
}

fn user_manager_uid(comp: &str) -> Option<u32> {
    comp.strip_prefix("user@")?.strip_suffix(".service")?.parse().ok()
}

enum Shape {
    Cause(Cause),
    /// systemd's slices and scopes, but nothing that names a unit or an app.
    Generic,
    NotSystemd,
}

fn from_cgroup(path: &str, pid: u32) -> Shape {
    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let container = |runtime, id| Shape::Cause(Cause::Container { runtime, id });
    if comps.iter().any(|c| c.starts_with("kubepods")) {
        let id = comps.last().and_then(|c| {
            scope_id(c, &["cri-containerd-", "crio-", "docker-"]).or_else(|| is_hex_id(c).then(|| c.to_string()))
        });
        return container(Runtime::Kubernetes, id);
    }
    for (i, c) in comps.iter().enumerate() {
        if let Some(id) = scope_id(c, &["docker-"]) {
            return container(Runtime::Docker, Some(id));
        }
        if *c == "docker" && comps.get(i + 1).is_some_and(|id| is_hex_id(id)) {
            return container(Runtime::Docker, comps.get(i + 1).map(|s| s.to_string()));
        }
        if let Some(id) = scope_id(c, &["libpod-conmon-", "libpod-payload-", "libpod-"]) {
            return container(Runtime::Podman, Some(id));
        }
        if *c == "machine.slice"
            && let Some(name) = comps
                .get(i + 1)
                .and_then(|m| m.strip_prefix("machine-")?.strip_suffix(".scope"))
        {
            return container(Runtime::Machine, Some(unescape(name)));
        }
    }
    if let Some(rest) = comps.iter().find_map(|c| c.strip_prefix("app-flatpak-")) {
        let rest = rest.strip_suffix(".scope").unwrap_or(rest);
        let app = match rest.rsplit_once('-') {
            Some((app, n)) if n.bytes().all(|b| b.is_ascii_digit()) => app,
            _ => rest,
        };
        return Shape::Cause(Cause::Flatpak { app: unescape(app) });
    }
    if let Some(name) = comps
        .iter()
        .find_map(|c| c.strip_prefix("snap.")?.split('.').next())
        .filter(|n| !n.is_empty())
    {
        return Shape::Cause(Cause::Snap { name: unescape(name) });
    }
    if let Some(id) = comps
        .iter()
        .find_map(|c| c.strip_prefix("session-")?.strip_suffix(".scope"))
    {
        return Shape::Cause(Cause::Session { id: id.to_owned() });
    }
    let manager = comps.iter().position(|c| user_manager_uid(c).is_some());
    let uid = manager.and_then(|i| user_manager_uid(comps[i]));
    if comps.last() == Some(&"init.scope") {
        return match manager {
            Some(i) => Shape::Cause(Cause::Service(Unit {
                name: comps[i].to_owned(),
                uid: None,
            })),
            None if pid == 1 => Shape::Cause(Cause::Init),
            None => Shape::Generic,
        };
    }
    let unit = comps
        .iter()
        .enumerate()
        .rev()
        .find(|(i, c)| (c.ends_with(".service") || c.ends_with(".scope")) && Some(*i) != manager);
    let Some((_, name)) = unit else {
        let systemd = comps.iter().any(|c| c.ends_with(".slice") || manager.is_some());
        return if systemd { Shape::Generic } else { Shape::NotSystemd };
    };
    let unit = Unit {
        name: (*name).to_owned(),
        uid,
    };
    if name.starts_with("app-") {
        let desktop_id = app_desktop_id(name);
        return Shape::Cause(if name.ends_with("@autostart.service") {
            Cause::Autostart { unit, desktop_id }
        } else {
            Cause::App { unit, desktop_id }
        });
    }
    if name.ends_with(".scope") {
        return Shape::Generic;
    }
    Shape::Cause(Cause::Service(unit))
}

fn is_terminal(comm: &str) -> bool {
    TERMINALS
        .iter()
        .any(|t| comm == &t[..t.len().min(15)] || comm.starts_with("tmux"))
}

fn terminal_path(chain: &[Link], need_emulator: bool) -> Option<Cause> {
    let outermost = |pred: fn(&str) -> bool| chain.iter().skip(1).rposition(|l| pred(&l.comm)).map(|i| i + 1);
    let (i, emulator) = match outermost(is_terminal) {
        Some(i) => (i, true),
        None if !need_emulator => (outermost(|c| SHELLS.contains(&c))?, false),
        None => return None,
    };
    Some(Cause::Terminal {
        path: chain[..=i].iter().rev().map(|l| l.comm.clone()).collect(),
        emulator,
    })
}

fn under_cron(chain: &[Link]) -> bool {
    chain.iter().skip(1).any(|l| CRON.contains(&l.comm.as_str()))
}

/// The cause from the cgroup file's text and the ancestors (the process first, PID 1 last). The cgroup is read first
/// because an orphan is adopted by PID 1 or a subreaper, so the parent chain alone misleads. Whether a service was
/// socket-, timer- or D-Bus-activated needs its unit properties, which `explain` adds.
pub fn classify(cgroup: &str, chain: &[Link]) -> Cause {
    classify_with(cgroup, chain, false)
}

/// `leader_runs`: the process's session leader is still alive, so an adopted process is no orphan.
fn classify_with(cgroup: &str, chain: &[Link], leader_runs: bool) -> Cause {
    let pid = chain.first().map_or(0, |l| l.pid);
    if chain.iter().any(|l| l.pid == 2) {
        return Cause::Kernel;
    }
    let path = cgroup_path(cgroup);
    let shape = path.map_or(Shape::NotSystemd, |p| from_cgroup(p, pid));
    match shape {
        Shape::Cause(Cause::Service(unit)) if under_cron(chain) && CRON.iter().any(|c| unit.name.starts_with(c)) => {
            Cause::Cron
        }
        Shape::Cause(c @ (Cause::App { .. } | Cause::Autostart { .. })) => terminal_path(chain, true).unwrap_or(c),
        Shape::Cause(c) => c,
        shape => {
            if under_cron(chain) {
                return Cause::Cron;
            }
            if let Some(t) = terminal_path(chain, false) {
                return t;
            }
            let parent = chain.get(1);
            let adopted = parent.is_some_and(|p| p.pid == 1 || p.comm == "systemd");
            match (shape, path) {
                (Shape::Generic, _) if adopted && !leader_runs => Cause::Orphan,
                (Shape::Generic, Some(p)) => Cause::Unknown {
                    reason: format!(
                        "its cgroup {p} names no service or app, and its parents show no terminal or cron job"
                    ),
                },
                (_, Some(p)) if p.len() > 1 => Cause::Unknown {
                    reason: format!(
                        "its cgroup {p} names no systemd unit, and its parents show no terminal or cron job"
                    ),
                },
                (_, Some(_)) => Cause::Unknown {
                    reason: "it is in the root cgroup, and its parents show no terminal or cron job".into(),
                },
                _ => Cause::Unknown {
                    reason: "its cgroup could not be read, and its parents show no terminal or cron job".into(),
                },
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UnitProps {
    pub id: String,
    pub description: String,
    pub fragment_path: String,
    pub source_path: String,
    pub unit_file_state: String,
    pub wanted_by: Vec<String>,
    pub triggered_by: Vec<String>,
    pub restart: String,
    pub n_restarts: Option<u32>,
    pub main_pid: Option<u32>,
    pub bus_name: String,
    /// Microseconds since the Unix epoch.
    pub active_enter: Option<u64>,
    /// Stopping the unit stops a unit that BindsTo `graphical-session.target`, which is how a compositor ends the session.
    pub holds_session: bool,
    /// A timer's `LastTriggerUSec` and `NextElapseUSecRealtime`; zero is `None`.
    pub last_trigger: Option<u64>,
    pub next_elapse: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    pub name: String,
    pub kind: String,
    pub remote: bool,
    pub remote_host: String,
    pub tty: String,
}

/// Everything `explain` reads from the machine; tests put a fake in its place.
pub trait Probe {
    fn proc_file(&self, pid: u32, name: &str) -> io::Result<String>;
    fn read(&self, path: &Path) -> io::Result<String>;
    fn data_dirs(&self) -> Vec<PathBuf>;
    fn uid(&self) -> u32;
    fn unit(&self, unit: &Unit) -> Result<UnitProps, String>;
    fn session(&self, id: &str) -> Result<Session, String>;
    fn docker_name(&self, id: &str) -> Result<String, String>;
    /// Where `/proc/<pid>/exe` points; only readable for the user's own processes.
    fn exe_link(&self, pid: u32) -> Option<String>;
    /// "at 09:12", or "on Oct 20 at 09:12" for another day.
    fn clock(&self, unix: u64) -> String {
        self.phrase(unix, clock_phrase)
    }
    /// "at 03:00", "03:00 tomorrow", or "on Oct 24 at 03:00".
    fn next_run(&self, unix: u64) -> String {
        self.phrase(unix, next_phrase)
    }
    fn phrase(&self, unix: u64, f: fn(&jiff::Zoned, &jiff::Zoned) -> String) -> String {
        let tz = jiff::tz::TimeZone::system();
        let Ok(ts) = jiff::Timestamp::from_second(unix as i64) else {
            return String::new();
        };
        f(&ts.to_zoned(tz.clone()), &jiff::Timestamp::now().to_zoned(tz))
    }
}

fn next_phrase(at: &jiff::Zoned, now: &jiff::Zoned) -> String {
    let time = at.strftime("%H:%M");
    if at.date() == now.date() {
        format!("at {time}")
    } else if now.date().tomorrow().is_ok_and(|d| d == at.date()) {
        format!("{time} tomorrow")
    } else {
        at.strftime("on %b %-d at %H:%M").to_string()
    }
}

fn clock_phrase(start: &jiff::Zoned, now: &jiff::Zoned) -> String {
    if start.date() == now.date() {
        start.strftime("at %H:%M").to_string()
    } else {
        start.strftime("on %b %-d at %H:%M").to_string()
    }
}

pub struct Live {
    pub proc_root: PathBuf,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
        }
    }
}

fn bus(system: bool) -> Result<zbus::blocking::Connection, String> {
    let builder = if system {
        zbus::blocking::connection::Builder::system()
    } else {
        zbus::blocking::connection::Builder::session()
    };
    builder.and_then(|b| b.method_timeout(TIMEOUT).build()).map_err(|e| {
        format!(
            "the {} bus did not answer ({e})",
            if system { "system" } else { "session" }
        )
    })
}

type Props = HashMap<String, zbus::zvariant::OwnedValue>;

fn get_all(bus: &zbus::blocking::Connection, dest: &str, path: &str, iface: &str) -> Result<Props, String> {
    bus.call_method(
        Some(dest),
        path,
        Some("org.freedesktop.DBus.Properties"),
        "GetAll",
        &(iface,),
    )
    .and_then(|m| m.body().deserialize::<Props>())
    .map_err(|e| e.to_string())
}

fn take<T: TryFrom<zbus::zvariant::OwnedValue>>(props: &mut Props, key: &str) -> Option<T> {
    props.remove(key).and_then(|v| T::try_from(v).ok())
}

fn object_path(
    bus: &zbus::blocking::Connection,
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
    arg: &str,
) -> Result<String, String> {
    bus.call_method(Some(dest), path, Some(iface), method, &(arg,))
        .and_then(|m| m.body().deserialize::<zbus::zvariant::OwnedObjectPath>())
        .map(|p| p.as_str().to_owned())
        .map_err(|e| e.to_string())
}

const SESSION_TARGET: &str = "graphical-session.target";

/// Whether stopping a unit takes the graphical session down: among the units that stop with it (`RequiredBy`,
/// `BoundBy`) is the session target itself or a unit that BindsTo it. `PartOf` alone never counts.
fn holds_session(stopped_with: &[String], binds_to: impl Fn(&str) -> Vec<String>) -> bool {
    stopped_with.iter().any(|n| n == SESSION_TARGET)
        || stopped_with
            .iter()
            .take(8)
            .any(|name| binds_to(name).iter().any(|n| n == SESSION_TARGET))
}

fn binds_to(bus: &zbus::blocking::Connection, name: &str) -> Vec<String> {
    let Ok(path) = object_path(
        bus,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "GetUnit",
        name,
    ) else {
        return Vec::new();
    };
    get_all(bus, "org.freedesktop.systemd1", &path, "org.freedesktop.systemd1.Unit")
        .ok()
        .and_then(|mut p| take(&mut p, "BindsTo"))
        .unwrap_or_default()
}

/// The port of a TCP or UDP `Listen` entry of a socket unit (`0.0.0.0:631`, `[::]:631`, `[fe80::1%eth0]:53`);
/// `None` for a path, netlink, vsock or any other kind.
fn listen_port(kind: &str, address: &str) -> Option<u16> {
    if kind != "Stream" && kind != "Datagram" {
        return None;
    }
    let (host, port) = address.rsplit_once(':')?;
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    let host = host.split_once('%').map_or(host, |(h, _)| h);
    host.parse::<IpAddr>().ok()?;
    port.parse().ok()
}

/// The first of `units` (name, `Listen` entries) listening on `port`.
pub fn socket_on_port(units: &[(String, Vec<(String, String)>)], port: u16) -> Option<&str> {
    units
        .iter()
        .find(|(_, listen)| listen.iter().any(|(kind, addr)| listen_port(kind, addr) == Some(port)))
        .map(|(name, _)| name.as_str())
}

/// The loaded `.socket` unit of the system or session manager listening on `port`, and the units it starts.
pub fn socket_unit(system: bool, port: u16) -> Result<Option<(String, Vec<String>)>, String> {
    type Listed = (
        String,
        String,
        String,
        String,
        String,
        String,
        zbus::zvariant::OwnedObjectPath,
        u32,
        String,
        zbus::zvariant::OwnedObjectPath,
    );
    const SYSTEMD: &str = "org.freedesktop.systemd1";
    let bus = bus(system)?;
    let listed: Vec<Listed> = bus
        .call_method(
            Some(SYSTEMD),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "ListUnitsByPatterns",
            &(Vec::<&str>::new(), vec!["*.socket"]),
        )
        .and_then(|m| m.body().deserialize())
        .map_err(|e| e.to_string())?;
    let units: Vec<(String, Vec<(String, String)>)> = listed
        .iter()
        .filter_map(|u| {
            let mut p = get_all(&bus, SYSTEMD, u.6.as_str(), "org.freedesktop.systemd1.Socket").ok()?;
            Some((u.0.clone(), take(&mut p, "Listen")?))
        })
        .collect();
    let Some(name) = socket_on_port(&units, port) else {
        return Ok(None);
    };
    let path = &listed.iter().find(|u| u.0 == name).expect("listed above").6;
    let triggers = get_all(&bus, SYSTEMD, path.as_str(), "org.freedesktop.systemd1.Unit")
        .ok()
        .and_then(|mut p| take(&mut p, "Triggers"))
        .unwrap_or_default();
    Ok(Some((name.to_owned(), triggers)))
}

impl Probe for Live {
    fn exe_link(&self, pid: u32) -> Option<String> {
        let link = std::fs::read_link(self.proc_root.join(pid.to_string()).join("exe")).ok()?;
        Some(link.to_string_lossy().into_owned())
    }

    fn proc_file(&self, pid: u32, name: &str) -> io::Result<String> {
        std::fs::read_to_string(self.proc_root.join(pid.to_string()).join(name))
    }

    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn data_dirs(&self) -> Vec<PathBuf> {
        data_dirs()
    }

    fn uid(&self) -> u32 {
        unsafe { libc::getuid() }
    }

    fn unit(&self, unit: &Unit) -> Result<UnitProps, String> {
        let bus = bus(unit.uid.is_none())?;
        let path = object_path(
            &bus,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "GetUnit",
            &unit.name,
        )?;
        let mut u = get_all(&bus, "org.freedesktop.systemd1", &path, "org.freedesktop.systemd1.Unit")?;
        let mut s = get_all(
            &bus,
            "org.freedesktop.systemd1",
            &path,
            "org.freedesktop.systemd1.Service",
        )
        .unwrap_or_default();
        let mut t = if unit.name.ends_with(".timer") {
            get_all(
                &bus,
                "org.freedesktop.systemd1",
                &path,
                "org.freedesktop.systemd1.Timer",
            )
            .unwrap_or_default()
        } else {
            Props::new()
        };
        let stopped_with: Vec<String> = ["RequiredBy", "BoundBy"]
            .iter()
            .flat_map(|k| take::<Vec<String>>(&mut u, k).unwrap_or_default())
            .collect();
        let nonzero = |v: Option<u64>| v.filter(|v| *v > 0);
        Ok(UnitProps {
            holds_session: holds_session(&stopped_with, |n| binds_to(&bus, n)),
            last_trigger: nonzero(take(&mut t, "LastTriggerUSec")),
            next_elapse: nonzero(take(&mut t, "NextElapseUSecRealtime")),
            id: take(&mut u, "Id").unwrap_or_default(),
            description: take(&mut u, "Description").unwrap_or_default(),
            fragment_path: take(&mut u, "FragmentPath").unwrap_or_default(),
            source_path: take(&mut u, "SourcePath").unwrap_or_default(),
            unit_file_state: take(&mut u, "UnitFileState").unwrap_or_default(),
            wanted_by: take(&mut u, "WantedBy").unwrap_or_default(),
            triggered_by: take(&mut u, "TriggeredBy").unwrap_or_default(),
            active_enter: take(&mut u, "ActiveEnterTimestamp").filter(|t| *t > 0),
            restart: take(&mut s, "Restart").unwrap_or_default(),
            n_restarts: take(&mut s, "NRestarts"),
            main_pid: take(&mut s, "MainPID"),
            bus_name: take(&mut s, "BusName").unwrap_or_default(),
        })
    }

    fn session(&self, id: &str) -> Result<Session, String> {
        let bus = bus(true)?;
        let path = object_path(
            &bus,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            "GetSession",
            id,
        )?;
        let mut p = get_all(&bus, "org.freedesktop.login1", &path, "org.freedesktop.login1.Session")?;
        Ok(Session {
            name: take(&mut p, "Name").unwrap_or_default(),
            kind: take(&mut p, "Type").unwrap_or_default(),
            remote: take(&mut p, "Remote").unwrap_or_default(),
            remote_host: take(&mut p, "RemoteHost").unwrap_or_default(),
            tty: take(&mut p, "TTY").unwrap_or_default(),
        })
    }

    fn docker_name(&self, id: &str) -> Result<String, String> {
        if !is_hex_id(id) {
            return Err("not a container id".into());
        }
        let mut sock = UnixStream::connect(DOCKER_SOCKET).map_err(|e| format!("{DOCKER_SOCKET}: {e}"))?;
        sock.set_read_timeout(Some(TIMEOUT)).ok();
        sock.set_write_timeout(Some(TIMEOUT)).ok();
        write!(sock, "GET /containers/{id}/json HTTP/1.0\r\nHost: docker\r\n\r\n").map_err(|e| e.to_string())?;
        let mut reply = Vec::new();
        sock.read_to_end(&mut reply).map_err(|e| e.to_string())?;
        let reply = String::from_utf8_lossy(&reply);
        let (head, body) = reply.split_once("\r\n\r\n").ok_or("no reply")?;
        if !head.starts_with("HTTP/1.0 200") && !head.starts_with("HTTP/1.1 200") {
            return Err(head.lines().next().unwrap_or("no reply").to_owned());
        }
        let json: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
        json["Name"]
            .as_str()
            .map(|n| n.trim_start_matches('/').to_owned())
            .ok_or_else(|| "no name in the reply".into())
    }
}

fn parse_stat(stat: &str) -> Option<(String, u32)> {
    let (open, close) = (stat.find('(')?, stat.rfind(')')?);
    let ppid = stat.get(close + 2..)?.split(' ').nth(1)?.parse().ok()?;
    Some((stat[open + 1..close].to_owned(), ppid))
}

/// The session id (field 6), start time (field 22) and state of a `/proc/<pid>/stat` line.
fn parse_session(stat: &str) -> Option<(u32, u64, char)> {
    let rest: Vec<&str> = stat.get(stat.rfind(')')? + 2..)?.split(' ').collect();
    Some((
        rest.get(3)?.parse().ok()?,
        rest.get(19)?.parse().ok()?,
        rest.first()?.chars().next()?,
    ))
}

/// Whether another process that leads this one's session is still alive. A reused pid starts after the process.
fn session_leader_runs(probe: &dyn Probe, pid: u32) -> bool {
    let read = |p| probe.proc_file(p, "stat").ok().and_then(|s| parse_session(&s));
    let Some((sid, start, _)) = read(pid) else {
        return false;
    };
    sid != 0 && sid != pid && read(sid).is_some_and(|(_, leader_start, state)| state != 'Z' && leader_start <= start)
}

fn build_chain(probe: &dyn Probe, pid: u32, table: &[Process]) -> Vec<Link> {
    let mut chain = Vec::new();
    let mut cur = pid;
    while cur != 0 && chain.len() < MAX_CHAIN && chain.iter().all(|l: &Link| l.pid != cur) {
        let (comm, ppid, exe) = match table.iter().find(|p| p.pid == cur) {
            Some(p) => (p.comm.clone(), p.ppid, p.exe.clone()),
            None => match probe.proc_file(cur, "stat").ok().and_then(|s| parse_stat(&s)) {
                Some((comm, ppid)) => (comm, ppid, None),
                None => break,
            },
        };
        chain.push(Link { pid: cur, comm, exe });
        cur = ppid;
    }
    chain
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Actions {
    /// The unit behind the process, for "Logs for this unit" and the three actions.
    pub unit: Option<String>,
    pub user: bool,
    pub stop: bool,
    pub restart: bool,
    pub disable: bool,
    /// For a container: the command to copy, never run.
    pub container_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Why {
    pub cause: Cause,
    pub sentence: String,
    /// At most four lines, the cause first and the process last.
    pub chain: Vec<String>,
    pub if_you_end: Option<String>,
    pub evidence: Vec<(String, String)>,
    pub notes: Vec<String>,
    /// Extra facts under the sentence.
    pub facts: Vec<String>,
    pub actions: Actions,
}

/// Blocking; call it off the UI thread, once per selected `pid` + `start_time`.
pub fn explain(pid: u32, start_time: u64, table: &[Process]) -> Why {
    explain_with(&Live::default(), pid, start_time, table)
}

#[derive(Default)]
struct Facts {
    props: Option<UnitProps>,
    session: Option<Session>,
    desktop: Option<(String, PathBuf)>,
    container_name: Option<String>,
    timer: Option<UnitProps>,
}

pub fn explain_with(probe: &dyn Probe, pid: u32, start_time: u64, table: &[Process]) -> Why {
    let mut notes = Vec::new();
    let unavailable = |notes: &mut Vec<String>, why: String| notes.push(format!("details unavailable: {why}"));
    let gone = || {
        let cause = Cause::Unknown {
            reason: "the process has exited".into(),
        };
        Why {
            sentence: "sysman can't tell why: the process has exited.".into(),
            cause,
            chain: Vec::new(),
            if_you_end: None,
            evidence: Vec::new(),
            notes: Vec::new(),
            facts: Vec::new(),
            actions: Actions::default(),
        }
    };
    if table
        .iter()
        .find(|p| p.pid == pid)
        .is_some_and(|p| p.start_time != 0 && start_time != 0 && p.start_time != start_time)
    {
        return gone();
    }
    let chain = build_chain(probe, pid, table);
    if chain.first().is_none_or(|l| l.pid != pid) {
        return gone();
    }
    let cgroup = match probe.proc_file(pid, "cgroup") {
        Ok(text) => text,
        Err(e) => {
            unavailable(&mut notes, format!("cgroup could not be read ({e})"));
            String::new()
        }
    };
    let mut cause = classify_with(&cgroup, &chain, false);
    if cause == Cause::Orphan && session_leader_runs(probe, pid) {
        cause = classify_with(&cgroup, &chain, true);
    }
    let mut facts = Facts::default();

    if let Some(unit) = unit_of(&cause).filter(|u| u.name.ends_with(".service")) {
        if unit.uid.is_some_and(|u| u != probe.uid()) {
            unavailable(&mut notes, "the unit belongs to another user's manager".into());
        } else {
            match probe.unit(unit) {
                Ok(p) => facts.props = Some(p),
                Err(e) => unavailable(&mut notes, format!("unit properties ({e})")),
            }
        }
    }
    if let Some(props) = &facts.props {
        cause = refine(cause, props);
    }
    match &cause {
        Cause::Timer { unit, timer } => {
            facts.timer = probe
                .unit(&Unit {
                    name: timer.clone(),
                    uid: unit.uid,
                })
                .ok()
        }
        Cause::Session { id } => match probe.session(id) {
            Ok(s) => facts.session = Some(s),
            Err(e) => unavailable(&mut notes, format!("login session ({e})")),
        },
        Cause::App { desktop_id, .. } | Cause::Autostart { desktop_id, .. } => {
            let source = facts
                .props
                .as_ref()
                .map(|p| p.source_path.as_str())
                .filter(|s| !s.is_empty());
            facts.desktop = desktop_entry(probe, &desktop_candidates(desktop_id), source);
            if facts.desktop.is_none() {
                unavailable(
                    &mut notes,
                    format!("no .desktop file found for {}", unescape(desktop_id)),
                );
            }
        }
        Cause::Flatpak { app } => facts.desktop = desktop_entry(probe, std::slice::from_ref(app), None),
        Cause::Container { runtime, id: Some(id) } => match container_name(probe, *runtime, id) {
            Ok(n) => facts.container_name = n,
            Err(e) => unavailable(&mut notes, format!("container name ({e})")),
        },
        _ => {}
    }

    let me = chain
        .first()
        .map_or_else(String::new, |l| format!("{} {}", l.comm, l.pid));
    let replaced = probe.exe_link(pid).is_some_and(|e| e.ends_with(" (deleted)"));
    Why {
        facts: replaced
            .then(|| "Running an older copy of this program; restart it to use the installed one.".to_owned())
            .into_iter()
            .collect(),
        sentence: sentence(&cause, &facts, probe, start_time),
        chain: chain_lines(&cause, &facts, &chain, &me),
        if_you_end: if_you_end(&cause, facts.props.as_ref(), pid),
        evidence: evidence(&cause, &facts, &cgroup, &chain, probe),
        actions: actions(&cause, &facts, probe, id_of(&cause)),
        cause,
        notes,
    }
}

/// Why a recorded process ran, from its recorded cgroup and ancestors; for one that has exited. Unit details are
/// looked up now, only if the unit still exists, and the sentence says so. No actions: there is nothing to act on.
pub fn explain_recorded(origin: &Origin, start_time: u64) -> Why {
    explain_recorded_with(&Live::default(), origin, start_time)
}

/// How long after its last recorded reading a process still counts as running.
const RUNNING_S: i64 = 30;

/// One line on why a recorded process ran, for the background executor: the live explanation if it is still
/// running, else the one rebuilt from the record.
pub fn recorded_line(history: &History, p: &Proc, now: i64) -> String {
    if now - p.last_t <= RUNNING_S {
        return explain(p.pid as u32, p.start_time as u64, &[]).sentence;
    }
    match history.origin(p.id) {
        Ok(Some(o)) => explain_recorded(&o, p.start_time as u64).sentence,
        _ => NOT_RECORDED.into(),
    }
}

pub const NOT_RECORDED: &str = "Not recorded (sysman was older then)";

pub fn explain_recorded_with(probe: &dyn Probe, origin: &Origin, start_time: u64) -> Why {
    let chain: Vec<Link> = origin
        .chain
        .iter()
        .map(|(pid, comm)| Link {
            pid: *pid,
            comm: comm.clone(),
            exe: None,
        })
        .collect();
    let mut cause = classify(&format!("0::{}\n", origin.cgroup), &chain);
    let mut facts = Facts::default();
    if let Some(unit) =
        unit_of(&cause).filter(|u| u.name.ends_with(".service") && u.uid.is_none_or(|uid| uid == probe.uid()))
    {
        facts.props = probe.unit(unit).ok();
    }
    if let Some(props) = &facts.props {
        cause = refine(cause, props);
    }
    if let Cause::Timer { unit, timer } = &cause {
        facts.timer = probe
            .unit(&Unit {
                name: timer.clone(),
                uid: unit.uid,
            })
            .ok();
    }
    let mut sentence = sentence(&cause, &facts, probe, start_time);
    if facts.props.is_some() {
        sentence = format!("{} (as of now)", sentence.trim_end_matches('.')) + ".";
    }
    let me = chain
        .first()
        .map_or_else(String::new, |l| format!("{} {}", l.comm, l.pid));
    Why {
        chain: chain_lines(&cause, &facts, &chain, &me),
        sentence,
        cause,
        if_you_end: None,
        evidence: Vec::new(),
        notes: Vec::new(),
        facts: Vec::new(),
        actions: Actions::default(),
    }
}

fn unit_of(cause: &Cause) -> Option<&Unit> {
    match cause {
        Cause::Service(unit)
        | Cause::Socket { unit, .. }
        | Cause::Timer { unit, .. }
        | Cause::DBus { unit, .. }
        | Cause::Autostart { unit, .. }
        | Cause::App { unit, .. } => Some(unit),
        _ => None,
    }
}

fn id_of(cause: &Cause) -> Option<(Runtime, &str)> {
    match cause {
        Cause::Container { runtime, id: Some(id) } => Some((*runtime, id)),
        _ => None,
    }
}

/// Socket, timer and D-Bus activation show only in the unit's properties.
fn refine(cause: Cause, p: &UnitProps) -> Cause {
    let Cause::Service(unit) = cause else {
        return cause;
    };
    let by = |suffix: &str| p.triggered_by.iter().find(|t| t.ends_with(suffix)).cloned();
    if let Some(socket) = by(".socket") {
        Cause::Socket { unit, socket }
    } else if let Some(timer) = by(".timer") {
        Cause::Timer { unit, timer }
    } else if !p.bus_name.is_empty() && p.wanted_by.is_empty() && p.triggered_by.is_empty() {
        Cause::DBus {
            unit,
            bus_name: p.bus_name.clone(),
        }
    } else {
        Cause::Service(unit)
    }
}

fn desktop_name(text: &str) -> Option<String> {
    let mut in_entry = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry && let Some(name) = line.strip_prefix("Name=") {
            return Some(name.trim().to_owned()).filter(|n| !n.is_empty());
        }
    }
    None
}

/// The entry's `Name=`, from the unit's own source file when it has one, else by the first desktop id that exists.
fn desktop_entry(probe: &dyn Probe, candidates: &[String], source: Option<&str>) -> Option<(String, PathBuf)> {
    let read = |path: PathBuf| probe.read(&path).ok().and_then(|t| desktop_name(&t)).map(|n| (n, path));
    if let Some(found) = source.and_then(|s| read(PathBuf::from(s))) {
        return Some(found);
    }
    let dirs = probe.data_dirs();
    candidates.iter().find_map(|c| {
        dirs.iter()
            .find_map(|d| read(d.join("applications").join(format!("{c}.desktop"))))
    })
}

fn container_name(probe: &dyn Probe, runtime: Runtime, id: &str) -> Result<Option<String>, String> {
    match runtime {
        Runtime::Docker => probe.docker_name(id).map(Some),
        Runtime::Podman => {
            let home = probe.data_dirs().into_iter().next().ok_or("no home directory")?;
            let path = home.join("containers/storage/overlay-containers/containers.json");
            let text = probe.read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let list: Vec<serde_json::Value> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            Ok(list
                .iter()
                .find(|c| c["id"].as_str().is_some_and(|i| i.starts_with(id) || id.starts_with(i)))
                .and_then(|c| c["names"][0].as_str())
                .map(str::to_owned))
        }
        _ => Ok(None),
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&format!("{home}/")) => format!("~{}", &path[home.len()..]),
        _ => path.to_owned(),
    }
}

fn enabled_phrase(state: &str, user: bool) -> &'static str {
    match state {
        "enabled" | "enabled-runtime" if user => ", enabled for your logins",
        "enabled" | "enabled-runtime" => ", enabled at boot",
        "static" => ", with no install section (static)",
        "transient" => ", a temporary unit",
        "generated" => ", generated at boot",
        _ => "",
    }
}

fn sentence(cause: &Cause, f: &Facts, probe: &dyn Probe, start_time: u64) -> String {
    let props = f.props.as_ref();
    let desc = |p: &UnitProps| (!p.description.is_empty()).then(|| p.description.clone());
    match cause {
        Cause::Init => "This is the init system, the first process the kernel starts.".into(),
        Cause::Kernel => "Part of the Linux kernel; it can't be ended.".into(),
        Cause::Service(unit) => {
            let who = match unit.uid {
                None => "System service".to_owned(),
                Some(uid) if uid == probe.uid() => "User service".to_owned(),
                Some(uid) => format!("Service of user {uid}"),
            };
            let name = unit.display();
            let label = props.and_then(desc).map_or(name.clone(), |d| format!("{d} ({name})"));
            let state = props.map_or("", |p| enabled_phrase(&p.unit_file_state, unit.uid.is_some()));
            match props.filter(|p| p.unit_file_state == "disabled") {
                Some(p) => {
                    let started = p
                        .active_enter
                        .map(|t| format!(", started {}", probe.clock(t / 1_000_000)));
                    format!(
                        "{who} {label}{}. It is not set to start automatically.",
                        started.unwrap_or_default()
                    )
                }
                None => format!("{who} {label}{state}."),
            }
        }
        Cause::Socket { socket, .. } => format!("Started when something connected to {}.", unescape(socket)),
        Cause::Timer { timer, .. } => {
            let (last, next) = f
                .timer
                .as_ref()
                .map_or((None, None), |t| (t.last_trigger, t.next_elapse));
            let mut out = format!("Started by the timer {}", unescape(timer));
            if let Some(last) = last {
                out += &format!(" {}", probe.clock(last / 1_000_000));
            }
            if let Some(next) = next {
                let sep = if last.is_some() { ";" } else { "" };
                out += &format!("{sep} next run {}", probe.next_run(next / 1_000_000));
            }
            out + "."
        }
        Cause::DBus { bus_name, .. } => format!("Started on demand for {bus_name}."),
        Cause::Autostart { desktop_id, .. } => match props.map(|p| p.source_path.as_str()).filter(|s| !s.is_empty()) {
            Some(src) => format!("Starts when you log in, from {}.", tilde(src)),
            None => format!(
                "Starts when you log in, from the autostart entry {}.",
                unescape(desktop_id)
            ),
        },
        Cause::App { desktop_id, .. } => {
            let when = probe.clock(start_time);
            match &f.desktop {
                Some((name, _)) => format!("You opened {name} {when}."),
                None => format!("Started as the desktop app {} {when}.", unescape(desktop_id)),
            }
        }
        Cause::Terminal { path, emulator } => format!(
            "Run {}: {}.",
            if *emulator { "in a terminal" } else { "from a shell" },
            path.join(" → ")
        ),
        Cause::Orphan => "Started by a terminal or script that has since exited.".into(),
        Cause::Session { id } => match &f.session {
            Some(s) => {
                let kind = match (s.remote, s.kind.as_str()) {
                    (true, _) => "remote login",
                    (_, "tty") => "console login",
                    (_, "x11" | "wayland" | "mir") => "graphical login",
                    _ => "login",
                };
                let host = if s.remote_host.is_empty() {
                    String::new()
                } else {
                    format!(" from {}", s.remote_host)
                };
                format!("Part of {}'s {kind}{host}.", s.name)
            }
            None => format!("Part of login session {id}."),
        },
        Cause::Container { runtime, id } => {
            let what = runtime.label();
            match (&f.container_name, id) {
                (Some(name), _) => format!("Runs in the {what} {name}."),
                (None, Some(id)) if *runtime == Runtime::Machine => format!("Runs in the {what} {id}."),
                (None, Some(id)) => format!("Runs in a {what}, {}.", short(id)),
                (None, None) => format!("Runs in a {what}."),
            }
        }
        Cause::Flatpak { app } => match &f.desktop {
            Some((name, _)) => format!("Flatpak app {name} ({app})."),
            None => format!("Flatpak app {app}."),
        },
        Cause::Snap { name } => format!("Snap app {name}."),
        Cause::Cron => "Started by cron.".into(),
        Cause::Unknown { reason } => format!("sysman can't tell why: {reason}."),
    }
}

fn chain_lines(cause: &Cause, f: &Facts, chain: &[Link], me: &str) -> Vec<String> {
    let props = f.props.as_ref();
    let unit_line = |unit: &Unit| {
        let name = unit.display();
        match props.and_then(|p| p.n_restarts) {
            Some(1) => format!("{name} · restarted 1 time"),
            Some(n) => format!("{name} · restarted {n} times"),
            None => name,
        }
    };
    let mut lines = match cause {
        Cause::Service(unit) => {
            let target = props.and_then(|p| {
                p.wanted_by
                    .iter()
                    .find(|w| w.ends_with(".target"))
                    .or(p.wanted_by.first())
                    .cloned()
            });
            target.into_iter().chain([unit_line(unit)]).collect()
        }
        Cause::Socket { unit, socket } => vec![unescape(socket), unit_line(unit)],
        Cause::Timer { unit, timer } => vec![unescape(timer), unit_line(unit)],
        Cause::DBus { unit, bus_name } => vec![format!("D-Bus {bus_name}"), unit_line(unit)],
        Cause::Autostart { unit, .. } => {
            let src = props.map(|p| tilde(&p.source_path)).filter(|s| !s.is_empty());
            src.into_iter().chain([unit_line(unit)]).collect()
        }
        Cause::App { unit, .. } => {
            let app = f.desktop.as_ref().map(|(n, _)| n.clone());
            app.into_iter().chain([unit.display()]).collect()
        }
        Cause::Terminal { path, .. } => path.iter().take(path.len().saturating_sub(1)).cloned().collect(),
        Cause::Orphan => chain
            .get(1)
            .map(|p| format!("{} {}", p.comm, p.pid))
            .into_iter()
            .collect(),
        Cause::Session { id } => vec![format!("session-{id}.scope")],
        Cause::Container { runtime, id } => {
            let name = f
                .container_name
                .clone()
                .or_else(|| id.as_deref().map(|i| short(i).to_owned()));
            vec![
                format!("{} {}", runtime.label(), name.unwrap_or_default())
                    .trim()
                    .to_owned(),
            ]
        }
        Cause::Flatpak { app } => vec![app.clone()],
        Cause::Snap { name } => vec![format!("snap {name}")],
        Cause::Cron => chain
            .iter()
            .skip(1)
            .find(|l| CRON.contains(&l.comm.as_str()))
            .map(|l| format!("{} {}", l.comm, l.pid))
            .into_iter()
            .collect(),
        Cause::Init | Cause::Kernel | Cause::Unknown { .. } => Vec::new(),
    };
    if lines.len() > 3 {
        let tail = lines.split_off(lines.len() - 1);
        lines.truncate(1);
        lines.push("…".into());
        lines.extend(tail);
    }
    lines.push(me.to_owned());
    lines
}

/// What ending the process does, only where the unit's properties make it known.
fn if_you_end(cause: &Cause, props: Option<&UnitProps>, pid: u32) -> Option<String> {
    if *cause == Cause::Kernel {
        return Some("It can't be ended.".into());
    }
    if !matches!(
        cause,
        Cause::Service(_) | Cause::Socket { .. } | Cause::Timer { .. } | Cause::DBus { .. } | Cause::Autostart { .. }
    ) {
        return None;
    }
    let p = props?;
    if p.holds_session && p.main_pid.is_none_or(|m| m == 0 || m == pid) {
        return Some("Ending it ends your desktop session.".into());
    }
    let main = p.main_pid.filter(|m| *m != 0)?;
    if main != pid {
        return Some(format!(
            "systemd does not restart the service when this process ends; only its main process, PID {main}, counts."
        ));
    }
    let until = match cause {
        Cause::Autostart { .. } => " until your next login".to_owned(),
        Cause::Socket { socket, .. } => format!(" until something connects to {}", unescape(socket)),
        Cause::DBus { bus_name, .. } => format!(" until something asks for {bus_name}"),
        _ => String::new(),
    };
    Some(match p.restart.as_str() {
        "always" => "systemd starts it again right away.".to_owned(),
        "on-failure" | "on-abnormal" | "on-abort" => {
            let again = if until.is_empty() { "" } else { " right away" };
            format!("After End it stays stopped{until}. After Force end, systemd starts it again{again}.")
        }
        "on-success" => {
            format!("After End, systemd starts it again right away. After Force end it stays stopped{until}.")
        }
        _ if !until.is_empty() => format!("It stays stopped{until}."),
        _ => "systemd does not start it again.".to_owned(),
    })
}

fn evidence(cause: &Cause, f: &Facts, cgroup: &str, chain: &[Link], probe: &dyn Probe) -> Vec<(String, String)> {
    let mut e = Vec::new();
    let mut add = |k: &str, v: String| {
        if !v.is_empty() {
            e.push((k.to_owned(), v));
        }
    };
    add("cgroup", cgroup_path(cgroup).unwrap_or_default().to_owned());
    if let Some(p) = &f.props {
        add("Unit file", p.fragment_path.clone());
        add("Source", p.source_path.clone());
        add("Enabled", p.unit_file_state.clone());
        add("WantedBy", p.wanted_by.join(", "));
        add("TriggeredBy", p.triggered_by.join(", "));
        add("Restart", p.restart.clone());
        add("NRestarts", p.n_restarts.map(|n| n.to_string()).unwrap_or_default());
        add("MainPID", p.main_pid.map(|n| n.to_string()).unwrap_or_default());
        add("BusName", p.bus_name.clone());
        add(
            "Active since",
            p.active_enter.map(|t| probe.clock(t / 1_000_000)).unwrap_or_default(),
        );
    }
    if let Some((_, path)) = &f.desktop {
        add("Desktop file", path.display().to_string());
    }
    if let Some(s) = &f.session {
        add("Session", format!("{} {} {}", s.name, s.kind, s.tty).trim().to_owned());
        add("Remote host", s.remote_host.clone());
    }
    if matches!(
        cause,
        Cause::Terminal { .. } | Cause::Orphan | Cause::Cron | Cause::Unknown { .. }
    ) {
        let parents: Vec<String> = chain.iter().skip(1).map(|l| format!("{} {}", l.comm, l.pid)).collect();
        add("Parents", parents.join(" ← "));
    }
    e
}

fn actions(cause: &Cause, f: &Facts, probe: &dyn Probe, container: Option<(Runtime, &str)>) -> Actions {
    if let Some((runtime, id)) = container {
        let tool = match runtime {
            Runtime::Docker => Some("docker"),
            Runtime::Podman => Some("podman"),
            _ => None,
        };
        return Actions {
            container_command: tool.filter(|_| is_hex_id(id)).map(|t| format!("{t} stop {id}")),
            ..Actions::default()
        };
    }
    let Some(unit) = unit_of(cause).filter(|u| u.name.ends_with(".service")) else {
        return Actions::default();
    };
    let mine = unit.uid.is_none_or(|u| u == probe.uid());
    let holds_session = f.props.as_ref().is_some_and(|p| p.holds_session);
    let valid = mine && restartable_unit(&unit.name) && !holds_session;
    let enabled = f
        .props
        .as_ref()
        .is_some_and(|p| matches!(p.unit_file_state.as_str(), "enabled" | "enabled-runtime" | "linked"));
    Actions {
        unit: Some(unit.name.clone()),
        user: unit.uid.is_some(),
        stop: valid,
        restart: valid,
        disable: valid && enabled,
        container_command: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: u32 = 1000;

    fn link(pid: u32, comm: &str) -> Link {
        Link {
            pid,
            comm: comm.into(),
            exe: None,
        }
    }

    fn chain(links: &[(u32, &str)]) -> Vec<Link> {
        links.iter().map(|(p, c)| link(*p, c)).collect()
    }

    #[test]
    fn a_waiting_port_is_traced_to_the_socket_unit_listening_on_it_over_ip_only() {
        let unit = |name: &str, listen: &[(&str, &str)]| {
            let listen = listen.iter().map(|(k, a)| (k.to_string(), a.to_string())).collect();
            (name.to_owned(), listen)
        };
        let units = vec![
            // a unix path that happens to end in the port number is not that port
            unit("odd.socket", &[("Stream", "/run/odd:631")]),
            unit("vsock.socket", &[("Stream", "vsock:2:631")]),
            unit("ssh.socket", &[("Stream", "[::]:22"), ("Stream", "0.0.0.0:2222")]),
            unit(
                "cups.socket",
                &[("Stream", "/run/cups/cups.sock"), ("Stream", "[::]:631")],
            ),
            unit("mdns.socket", &[("Datagram", "[fe80::1%eth0]:5353")]),
            unit("fifo.socket", &[("FIFO", "0.0.0.0:7000")]),
        ];
        assert_eq!(socket_on_port(&units, 631), Some("cups.socket"));
        assert_eq!(socket_on_port(&units, 2222), Some("ssh.socket"));
        assert_eq!(
            socket_on_port(&units, 5353),
            Some("mdns.socket"),
            "UDP and a scoped link-local address count"
        );
        assert_eq!(socket_on_port(&units, 7000), None, "a FIFO has no port");
        assert_eq!(socket_on_port(&units, 80), None);
    }

    fn plain() -> Vec<Link> {
        chain(&[(812, "app"), (1, "systemd")])
    }

    fn v2(path: &str) -> String {
        format!("0::{path}\n")
    }

    fn unit(name: &str, uid: Option<u32>) -> Unit {
        Unit { name: name.into(), uid }
    }

    #[test]
    fn a_system_service_is_named_by_its_cgroup() {
        let c = classify(&v2("/system.slice/sshd.service"), &plain());
        assert_eq!(c, Cause::Service(unit("sshd.service", None)));
    }

    #[test]
    fn a_service_in_a_sub_cgroup_is_still_that_service() {
        let c = classify(&v2("/system.slice/foo.service/worker"), &plain());
        assert_eq!(c, Cause::Service(unit("foo.service", None)));
    }

    #[test]
    fn a_service_under_the_users_manager_is_a_user_service() {
        let c = classify(
            &v2("/user.slice/user-1000.slice/user@1000.service/app.slice/pipewire.service"),
            &plain(),
        );
        assert_eq!(c, Cause::Service(unit("pipewire.service", Some(1000))));
    }

    #[test]
    fn the_user_manager_itself_is_a_system_service() {
        let c = classify(
            &v2("/user.slice/user-1000.slice/user@1000.service/init.scope"),
            &plain(),
        );
        assert_eq!(c, Cause::Service(unit("user@1000.service", None)));
    }

    #[test]
    fn escaped_unit_names_are_kept_for_systemctl_and_unescaped_for_display() {
        let c = classify(
            &v2("/system.slice/systemd-fsck@dev-disk-by\\x2duuid-ab.service"),
            &plain(),
        );
        let Cause::Service(u) = c else { panic!("{c:?}") };
        assert_eq!(u.name, "systemd-fsck@dev-disk-by\\x2duuid-ab.service");
        assert_eq!(u.display(), "systemd-fsck@dev-disk-by-uuid-ab.service");
    }

    #[test]
    fn a_v1_hybrid_is_read_from_its_systemd_line() {
        let text = "12:cpu,cpuacct:/\n1:name=systemd:/system.slice/cups.service\n0::/system.slice/cups.service\n";
        assert_eq!(classify(text, &plain()), Cause::Service(unit("cups.service", None)));
    }

    #[test]
    fn a_v1_hybrid_without_a_systemd_name_falls_back_to_the_v2_line() {
        let text = "1:name=systemd:/\n0::/system.slice/cups.service\n";
        assert_eq!(classify(text, &plain()), Cause::Service(unit("cups.service", None)));
    }

    #[test]
    fn an_openrc_path_is_answered_from_the_parent_chain() {
        let c = classify("0::/openrc.sshd\n", &chain(&[(300, "sshd"), (1, "init")]));
        let Cause::Unknown { reason } = c else { panic!("{c:?}") };
        assert!(reason.contains("names no systemd unit"), "{reason}");
        let t = classify(
            "1:name=openrc:/\n",
            &chain(&[(30, "ls"), (20, "bash"), (10, "xterm"), (1, "init")]),
        );
        assert_eq!(
            t,
            Cause::Terminal {
                path: vec!["xterm".into(), "bash".into(), "ls".into()],
                emulator: true
            }
        );
    }

    #[test]
    fn a_process_in_a_systemd_less_cgroup_adopted_by_init_is_not_called_an_orphan() {
        let c = classify("0::/\n", &chain(&[(300, "nginx"), (1, "init")]));
        assert!(matches!(c, Cause::Unknown { .. }), "{c:?}");
    }

    #[test]
    fn app_scopes_name_the_desktop_id() {
        let launcher = classify(
            &v2("/user.slice/user-1000.slice/user@1000.service/app.slice/app-Hyprland-firefox@abc.scope"),
            &plain(),
        );
        assert_eq!(
            launcher,
            Cause::App {
                unit: unit("app-Hyprland-firefox@abc.scope", Some(1000)),
                desktop_id: "Hyprland-firefox".into()
            }
        );
        let numbered = classify(
            &v2("/user.slice/user-1000.slice/user@1000.service/app.slice/app-firefox-1234.scope"),
            &plain(),
        );
        let Cause::App { desktop_id, .. } = numbered else {
            panic!("{numbered:?}")
        };
        assert_eq!(desktop_id, "firefox");
    }

    #[test]
    fn an_xdg_autostart_service_is_a_login_autostart() {
        let c = classify(
            &v2("/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-nm\\x2dapplet@autostart.service"),
            &plain(),
        );
        let Cause::Autostart { unit, desktop_id } = c else {
            panic!("{c:?}")
        };
        assert_eq!(unit.uid, Some(1000));
        assert_eq!(desktop_id, "gnome-nm\\x2dapplet");
    }

    #[test]
    fn a_command_run_in_a_terminals_app_scope_is_run_in_that_terminal() {
        let c = classify(
            &v2("/user.slice/user-1000.slice/user@1000.service/app.slice/app-kitty-4321.scope"),
            &chain(&[(900, "cargo"), (800, "zsh"), (700, "kitty"), (1, "systemd")]),
        );
        assert_eq!(
            c,
            Cause::Terminal {
                path: vec!["kitty".into(), "zsh".into(), "cargo".into()],
                emulator: true
            }
        );
    }

    #[test]
    fn an_app_started_by_a_launcher_script_is_not_called_a_terminal_command() {
        let c = classify(
            &v2("/user.slice/user-1000.slice/user@1000.service/app.slice/app-firefox-1.scope"),
            &chain(&[(900, "firefox"), (800, "sh"), (1, "systemd")]),
        );
        assert!(matches!(c, Cause::App { .. }), "{c:?}");
    }

    #[test]
    fn a_login_session_scope_names_the_session() {
        let c = classify(&v2("/user.slice/user-1000.slice/session-3.scope"), &plain());
        assert_eq!(c, Cause::Session { id: "3".into() });
    }

    #[test]
    fn pid_one_in_init_scope_is_the_init_system() {
        assert_eq!(classify(&v2("/init.scope"), &chain(&[(1, "systemd")])), Cause::Init);
    }

    #[test]
    fn a_process_adopted_by_the_users_manager_from_a_generic_scope_is_an_orphan() {
        let c = classify(
            &v2("/user.slice/user-1000.slice/user@1000.service/app.slice/run-r1a2b.scope"),
            &chain(&[(950, "server"), (900, "systemd"), (1, "systemd")]),
        );
        assert_eq!(c, Cause::Orphan);
    }

    #[test]
    fn a_generic_scope_under_a_shell_is_run_from_that_shell() {
        let c = classify(
            &v2("/user.slice/user-1000.slice/user@1000.service/app.slice/vte-spawn-1.scope"),
            &chain(&[(950, "make"), (940, "bash"), (1, "systemd")]),
        );
        assert_eq!(
            c,
            Cause::Terminal {
                path: vec!["bash".into(), "make".into()],
                emulator: false
            }
        );
    }

    #[test]
    fn container_cgroups_name_the_runtime_and_id() {
        let id = "3f2a9c1b7d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8";
        let docker = classify(&v2(&format!("/system.slice/docker-{id}.scope")), &plain());
        assert_eq!(
            docker,
            Cause::Container {
                runtime: Runtime::Docker,
                id: Some(id.into())
            }
        );
        let cgroupfs = classify(&v2(&format!("/docker/{id}")), &plain());
        assert_eq!(
            cgroupfs,
            Cause::Container {
                runtime: Runtime::Docker,
                id: Some(id.into())
            }
        );
        let podman = classify(
            &v2(&format!(
                "/user.slice/user-1000.slice/user@1000.service/user.slice/libpod-{id}.scope/container"
            )),
            &plain(),
        );
        assert_eq!(
            podman,
            Cause::Container {
                runtime: Runtime::Podman,
                id: Some(id.into())
            }
        );
        let kube = classify(
            &v2(&format!(
                "/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-podabc.slice/cri-containerd-{id}.scope"
            )),
            &plain(),
        );
        assert_eq!(
            kube,
            Cause::Container {
                runtime: Runtime::Kubernetes,
                id: Some(id.into())
            }
        );
        let machine = classify(&v2("/machine.slice/machine-arch\\x2dbox.scope/init.scope"), &plain());
        assert_eq!(
            machine,
            Cause::Container {
                runtime: Runtime::Machine,
                id: Some("arch-box".into())
            }
        );
    }

    #[test]
    fn flatpak_and_snap_scopes_name_the_app() {
        let flatpak = classify(
            &v2(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-com.discordapp.Discord-5532.scope",
            ),
            &plain(),
        );
        assert_eq!(
            flatpak,
            Cause::Flatpak {
                app: "com.discordapp.Discord".into()
            }
        );
        let snap = classify(
            &v2("/user.slice/user-1000.slice/user@1000.service/app.slice/snap.firefox.firefox-3a1f.scope"),
            &plain(),
        );
        assert_eq!(snap, Cause::Snap { name: "firefox".into() });
        let snap_service = classify(&v2("/system.slice/snap.lxd.daemon.service"), &plain());
        assert_eq!(snap_service, Cause::Snap { name: "lxd".into() });
    }

    #[test]
    fn jobs_run_by_cron_are_cron_even_though_the_cgroup_is_crons_service() {
        let c = classify(
            &v2("/system.slice/cronie.service"),
            &chain(&[(500, "backup.sh"), (400, "crond"), (1, "systemd")]),
        );
        assert_eq!(c, Cause::Cron);
        let daemon = classify(
            &v2("/system.slice/cronie.service"),
            &chain(&[(400, "crond"), (1, "systemd")]),
        );
        assert_eq!(daemon, Cause::Service(unit("cronie.service", None)));
    }

    #[test]
    fn kernel_threads_are_the_kernel() {
        assert_eq!(classify("0::/\n", &chain(&[(2, "kthreadd")])), Cause::Kernel);
        assert_eq!(
            classify("0::/\n", &chain(&[(15, "rcu_gp"), (2, "kthreadd")])),
            Cause::Kernel
        );
    }

    #[test]
    fn a_process_that_names_nothing_is_unknown_with_the_reason() {
        let c = classify("", &chain(&[(77, "mystery"), (50, "daemonish"), (1, "systemd")]));
        let Cause::Unknown { reason } = c else { panic!("{c:?}") };
        assert!(reason.contains("could not be read"), "{reason}");
    }

    #[test]
    fn a_process_name_alone_never_decides_the_cause() {
        let c = classify(
            &v2("/system.slice/foo.service"),
            &chain(&[(77, "firefox"), (1, "systemd")]),
        );
        assert_eq!(c, Cause::Service(unit("foo.service", None)));
    }

    #[test]
    fn unescape_decodes_hex_bytes_and_leaves_stray_backslashes() {
        assert_eq!(unescape("a\\x2db\\x2fc"), "a-b/c");
        assert_eq!(unescape("a\\xzz"), "a\\xzz");
        assert_eq!(unescape("caf\\xc3\\xa9"), "café");
    }

    fn props(restart: &str, main_pid: u32) -> UnitProps {
        UnitProps {
            restart: restart.into(),
            main_pid: Some(main_pid),
            ..UnitProps::default()
        }
    }

    fn service() -> Cause {
        Cause::Service(unit("x.service", None))
    }

    fn ending(cause: &Cause, p: &UnitProps) -> String {
        if_you_end(cause, Some(p), 10).expect("known")
    }

    #[test]
    fn an_always_restart_service_comes_back_after_end_and_force_end() {
        let s = ending(&service(), &props("always", 10));
        assert_eq!(s, "systemd starts it again right away.");
    }

    #[test]
    fn a_sigterm_does_not_bring_back_an_on_failure_service() {
        for restart in ["on-failure", "on-abnormal", "on-abort"] {
            let s = ending(&service(), &props(restart, 10));
            assert_eq!(
                s, "After End it stays stopped. After Force end, systemd starts it again.",
                "{restart}"
            );
        }
    }

    #[test]
    fn a_sigkill_does_not_bring_back_an_on_success_service() {
        let s = ending(&service(), &props("on-success", 10));
        assert_eq!(
            s,
            "After End, systemd starts it again right away. After Force end it stays stopped."
        );
    }

    #[test]
    fn ending_a_worker_that_is_not_the_main_process_restarts_nothing() {
        let s = ending(&service(), &props("always", 99));
        assert!(s.contains("only its main process, PID 99"), "{s}");
        assert!(!s.contains("starts it again"), "{s}");
    }

    #[test]
    fn nothing_is_claimed_when_the_units_properties_are_unknown() {
        assert_eq!(if_you_end(&service(), None, 10), None);
        assert_eq!(if_you_end(&service(), Some(&props("always", 0)), 10), None);
    }

    #[test]
    fn a_restart_no_service_says_nothing_starts_it_again() {
        let s = ending(&service(), &props("no", 10));
        assert_eq!(s, "systemd does not start it again.");
    }

    #[test]
    fn socket_dbus_and_autostart_units_say_how_they_start_again() {
        let socket = Cause::Socket {
            unit: unit("x.service", None),
            socket: "x.socket".into(),
        };
        assert!(ending(&socket, &props("no", 10)).contains("connects to x.socket"));
        let dbus = Cause::DBus {
            unit: unit("x.service", None),
            bus_name: "org.x.Y".into(),
        };
        assert!(ending(&dbus, &props("no", 10)).contains("asks for org.x.Y"));
        let auto = Cause::Autostart {
            unit: unit("app-x@autostart.service", Some(ME)),
            desktop_id: "x".into(),
        };
        assert!(ending(&auto, &props("no", 10)).contains("next login"));
    }

    #[test]
    fn a_restart_and_a_socket_both_get_said() {
        let socket = Cause::Socket {
            unit: unit("x.service", None),
            socket: "x.socket".into(),
        };
        let s = ending(&socket, &props("on-failure", 10));
        assert_eq!(
            s,
            "After End it stays stopped until something connects to x.socket. \
             After Force end, systemd starts it again right away."
        );
    }

    #[test]
    fn kernel_threads_say_they_cannot_be_ended_and_containers_claim_nothing() {
        assert_eq!(
            if_you_end(&Cause::Kernel, None, 2).as_deref(),
            Some("It can't be ended.")
        );
        let c = Cause::Container {
            runtime: Runtime::Docker,
            id: Some("abc".into()),
        };
        assert_eq!(if_you_end(&c, Some(&props("always", 10)), 10), None);
        assert_eq!(if_you_end(&Cause::Orphan, None, 10), None);
    }

    #[derive(Default)]
    struct Fake {
        files: HashMap<String, String>,
        units: HashMap<String, UnitProps>,
        sessions: HashMap<String, Session>,
        docker: Option<String>,
    }

    impl Fake {
        fn put(&mut self, path: &str, text: &str) {
            self.files.insert(path.into(), text.into());
        }
    }

    impl Probe for Fake {
        fn proc_file(&self, pid: u32, name: &str) -> io::Result<String> {
            self.read(Path::new(&format!("/proc/{pid}/{name}")))
        }

        fn read(&self, path: &Path) -> io::Result<String> {
            self.files
                .get(path.to_str().unwrap())
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }

        fn data_dirs(&self) -> Vec<PathBuf> {
            vec![PathBuf::from("/home/me/.local/share"), PathBuf::from("/usr/share")]
        }

        fn uid(&self) -> u32 {
            ME
        }

        fn unit(&self, unit: &Unit) -> Result<UnitProps, String> {
            self.units.get(&unit.name).cloned().ok_or_else(|| "no such unit".into())
        }

        fn session(&self, id: &str) -> Result<Session, String> {
            self.sessions.get(id).cloned().ok_or_else(|| "no such session".into())
        }

        fn docker_name(&self, _: &str) -> Result<String, String> {
            self.docker.clone().ok_or_else(|| "permission denied".into())
        }

        fn exe_link(&self, pid: u32) -> Option<String> {
            self.files.get(&format!("/proc/{pid}/exe")).cloned()
        }

        fn clock(&self, _: u64) -> String {
            "at 09:12".into()
        }

        fn next_run(&self, _: u64) -> String {
            "03:00 tomorrow".into()
        }
    }

    fn row(pid: u32, ppid: u32, comm: &str) -> Process {
        Process {
            pid,
            ppid,
            comm: comm.into(),
            start_time: 100,
            ..Process::default()
        }
    }

    fn table() -> Vec<Process> {
        vec![row(1, 0, "systemd"), row(812, 1, "sshd")]
    }

    fn sshd_probe(restart: &str) -> Fake {
        let mut f = Fake::default();
        f.files
            .insert("/proc/812/cgroup".into(), v2("/system.slice/sshd.service"));
        f.units.insert(
            "sshd.service".into(),
            UnitProps {
                description: "OpenSSH Daemon".into(),
                unit_file_state: "enabled".into(),
                wanted_by: vec!["multi-user.target".into()],
                restart: restart.into(),
                n_restarts: Some(0),
                main_pid: Some(812),
                ..UnitProps::default()
            },
        );
        f
    }

    #[test]
    fn a_system_service_reads_as_the_spec_sentence_with_its_chain_and_actions() {
        let w = explain_with(&sshd_probe("on-failure"), 812, 100, &table());
        assert_eq!(
            w.sentence,
            "System service OpenSSH Daemon (sshd.service), enabled at boot."
        );
        assert_eq!(
            w.chain,
            ["multi-user.target", "sshd.service · restarted 0 times", "sshd 812"]
        );
        assert!(
            w.if_you_end
                .as_deref()
                .unwrap()
                .contains("After Force end, systemd starts it again")
        );
        assert!(w.notes.is_empty(), "{:?}", w.notes);
        assert_eq!(
            w.actions,
            Actions {
                unit: Some("sshd.service".into()),
                user: false,
                stop: true,
                restart: true,
                disable: true,
                container_command: None
            }
        );
        assert!(
            w.evidence
                .iter()
                .any(|(k, v)| k == "cgroup" && v == "/system.slice/sshd.service")
        );
    }

    #[test]
    fn a_socket_triggered_service_is_started_when_something_connected() {
        let mut f = sshd_probe("no");
        f.units.get_mut("sshd.service").unwrap().triggered_by = vec!["sshd.socket".into()];
        let w = explain_with(&f, 812, 100, &table());
        assert_eq!(w.sentence, "Started when something connected to sshd.socket.");
        assert!(w.if_you_end.unwrap().contains("connects to sshd.socket"));
    }

    #[test]
    fn a_timer_triggered_service_names_the_timer() {
        let mut f = sshd_probe("no");
        f.units.get_mut("sshd.service").unwrap().triggered_by = vec!["man-db.timer".into()];
        let w = explain_with(&f, 812, 100, &table());
        assert_eq!(w.sentence, "Started by the timer man-db.timer.");
    }

    #[test]
    fn a_bus_name_nothing_wants_means_started_on_demand() {
        let mut f = sshd_probe("no");
        let p = f.units.get_mut("sshd.service").unwrap();
        p.wanted_by.clear();
        p.bus_name = "org.freedesktop.UDisks2".into();
        let w = explain_with(&f, 812, 100, &table());
        assert_eq!(w.sentence, "Started on demand for org.freedesktop.UDisks2.");
        let mut wanted = sshd_probe("no");
        wanted.units.get_mut("sshd.service").unwrap().bus_name = "org.x".into();
        assert!(
            explain_with(&wanted, 812, 100, &table())
                .sentence
                .starts_with("System service")
        );
    }

    #[test]
    fn missing_unit_details_are_noted_without_failing_the_explanation() {
        let mut f = sshd_probe("no");
        f.units.clear();
        let w = explain_with(&f, 812, 100, &table());
        assert_eq!(w.sentence, "System service sshd.service.");
        assert_eq!(w.notes, ["details unavailable: unit properties (no such unit)"]);
        assert_eq!(w.if_you_end, None);
        assert_eq!(w.chain, ["sshd.service", "sshd 812"]);
    }

    #[test]
    fn another_users_unit_is_not_queried_or_offered_actions() {
        let mut f = Fake::default();
        f.files.insert(
            "/proc/812/cgroup".into(),
            v2("/user.slice/user-1001.slice/user@1001.service/app.slice/foo.service"),
        );
        let w = explain_with(&f, 812, 100, &table());
        assert!(w.notes[0].contains("another user's manager"), "{:?}", w.notes);
        assert!(!w.actions.stop && !w.actions.restart && !w.actions.disable);
    }

    #[test]
    fn units_that_end_the_session_are_not_offered_to_stop() {
        let mut f = Fake::default();
        f.files
            .insert("/proc/812/cgroup".into(), v2("/system.slice/gdm.service"));
        let w = explain_with(&f, 812, 100, &table());
        assert!(!w.actions.stop && !w.actions.restart);
        assert_eq!(w.actions.unit.as_deref(), Some("gdm.service"));
    }

    #[test]
    fn an_app_scope_gets_its_name_from_the_desktop_file_past_a_launcher_prefix() {
        let mut f = Fake::default();
        f.files.insert(
            "/proc/812/cgroup".into(),
            v2("/user.slice/user-1000.slice/user@1000.service/app.slice/app-Hyprland-firefox@abc.scope"),
        );
        f.files.insert(
            "/usr/share/applications/firefox.desktop".into(),
            "[Desktop Entry]\nName=Firefox\nName[de]=Feuerfuchs\n".into(),
        );
        let w = explain_with(&f, 812, 100, &table());
        assert_eq!(w.sentence, "You opened Firefox at 09:12.");
        assert!(w.notes.is_empty());
    }

    #[test]
    fn an_autostart_entry_is_named_by_the_units_source_file() {
        let mut f = Fake::default();
        f.files.insert(
            "/proc/812/cgroup".into(),
            v2("/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo@autostart.service"),
        );
        f.units.insert(
            "app-foo@autostart.service".into(),
            UnitProps {
                source_path: "/home/me/.config/autostart/foo.desktop".into(),
                main_pid: Some(812),
                restart: "no".into(),
                ..UnitProps::default()
            },
        );
        let w = explain_with(&f, 812, 100, &table());
        assert!(
            w.sentence.starts_with("Starts when you log in, from "),
            "{}",
            w.sentence
        );
        assert!(
            w.sentence.ends_with("/.config/autostart/foo.desktop."),
            "{}",
            w.sentence
        );
        assert!(w.if_you_end.unwrap().contains("next login"));
    }

    #[test]
    fn a_login_session_names_the_user_and_remote_host() {
        let mut f = Fake::default();
        f.files.insert(
            "/proc/812/cgroup".into(),
            v2("/user.slice/user-1000.slice/session-3.scope"),
        );
        f.sessions.insert(
            "3".into(),
            Session {
                name: "mahfuz".into(),
                kind: "tty".into(),
                remote: true,
                remote_host: "192.168.1.5".into(),
                tty: "pts/0".into(),
            },
        );
        let w = explain_with(&f, 812, 100, &table());
        assert_eq!(w.sentence, "Part of mahfuz's remote login from 192.168.1.5.");
    }

    #[test]
    fn a_docker_container_shows_the_short_id_when_the_socket_is_not_readable() {
        let id = "3f2a9c1b7d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8";
        let mut f = Fake::default();
        f.files.insert(
            "/proc/812/cgroup".into(),
            v2(&format!("/system.slice/docker-{id}.scope")),
        );
        let w = explain_with(&f, 812, 100, &table());
        assert_eq!(w.sentence, "Runs in a Docker container, 3f2a9c1b7d4e.");
        assert_eq!(w.if_you_end, None);
        assert_eq!(
            w.actions.container_command.as_deref(),
            Some(format!("docker stop {id}").as_str())
        );
        assert!(
            w.notes[0].starts_with("details unavailable: container name"),
            "{:?}",
            w.notes
        );
        f.docker = Some("web".into());
        assert_eq!(
            explain_with(&f, 812, 100, &table()).sentence,
            "Runs in the Docker container web."
        );
    }

    #[test]
    fn a_podman_container_name_comes_from_containers_json() {
        let id = "aa11bb22cc33dd44ee55ff66";
        let mut f = Fake::default();
        f.files.insert(
            "/proc/812/cgroup".into(),
            v2(&format!("/user.slice/user@1000.service/libpod-{id}.scope")),
        );
        f.files.insert(
            "/home/me/.local/share/containers/storage/overlay-containers/containers.json".into(),
            format!(r#"[{{"id":"{id}","names":["db"]}}]"#),
        );
        assert_eq!(
            explain_with(&f, 812, 100, &table()).sentence,
            "Runs in the Podman container db."
        );
    }

    #[test]
    fn a_parent_missing_from_the_table_is_found_in_proc() {
        let mut f = Fake::default();
        f.files.insert(
            "/proc/812/cgroup".into(),
            v2("/user.slice/user-1000.slice/user@1000.service/app.slice/vte-spawn-1.scope"),
        );
        f.files
            .insert("/proc/700/stat".into(), "700 (bash) S 1 700 700 0".into());
        f.files.insert("/proc/1/stat".into(), "1 (systemd) S 0 1 1 0".into());
        let w = explain_with(&f, 812, 100, &[row(812, 700, "make")]);
        assert_eq!(w.sentence, "Run from a shell: bash → make.");
    }

    #[test]
    fn a_comm_with_parentheses_does_not_break_the_stat_parse() {
        assert_eq!(parse_stat("700 (my (sh) x) S 55 700"), Some(("my (sh) x".into(), 55)));
    }

    #[test]
    fn a_reused_pid_is_reported_as_exited() {
        let w = explain_with(&sshd_probe("no"), 812, 999, &table());
        assert_eq!(w.sentence, "sysman can't tell why: the process has exited.");
    }

    #[test]
    fn the_chain_is_at_most_four_lines_with_the_process_last() {
        let cause = Cause::Terminal {
            path: ["kitty", "zsh", "bash", "make", "cc"].map(String::from).to_vec(),
            emulator: true,
        };
        let lines = chain_lines(&cause, &Facts::default(), &[], "cc 9");
        assert_eq!(lines, ["kitty", "…", "make", "cc 9"]);
    }

    #[test]
    fn a_time_today_has_no_date_and_another_day_does() {
        let tz = jiff::tz::TimeZone::UTC;
        let at = |s: &str| s.parse::<jiff::Timestamp>().unwrap().to_zoned(tz.clone());
        assert_eq!(
            clock_phrase(&at("2026-10-22T09:12:00Z"), &at("2026-10-22T18:00:00Z")),
            "at 09:12"
        );
        assert_eq!(
            clock_phrase(&at("2026-10-20T09:12:00Z"), &at("2026-10-22T18:00:00Z")),
            "on Oct 20 at 09:12"
        );
    }

    const HOME_APPS: &str = "/usr/share/applications";

    fn app_probe(unit: &str) -> Fake {
        let mut f = Fake::default();
        f.put(
            "/proc/812/cgroup",
            &v2(&format!(
                "/user.slice/user-1000.slice/user@1000.service/app.slice/{unit}"
            )),
        );
        f
    }

    #[test]
    fn random_scope_suffixes_resolve_to_the_real_desktop_id() {
        for (unit, file, name) in [
            ("app-Hyprland-chromium-ff69785e.scope", "chromium", "Chromium"),
            (
                "app-Hyprland-google\\x2dchrome-0a1b2c.scope",
                "google-chrome",
                "Google Chrome",
            ),
            ("app-firefox-1234.scope", "firefox", "Firefox"),
            ("app-org.gnome.Nautilus-zzzz.scope", "org.gnome.Nautilus", "Files"),
            ("app-foo-bar-77.scope", "foo-bar", "Foo Bar"),
            ("app-Hyprland-firefox@a1b2.service", "firefox", "Firefox"),
        ] {
            let mut f = app_probe(unit);
            f.put(
                &format!("{HOME_APPS}/{file}.desktop"),
                &format!("[Desktop Entry]\nName={name}\n"),
            );
            let w = explain_with(&f, 812, 100, &table());
            assert_eq!(w.sentence, format!("You opened {name} at 09:12."), "{unit}");
        }
    }

    #[test]
    fn the_scope_suffix_is_dropped_whatever_it_looks_like() {
        assert_eq!(
            app_desktop_id("app-Hyprland-chromium-ff69785e.scope"),
            "Hyprland-chromium"
        );
        assert_eq!(app_desktop_id("app-chromium-12345.scope"), "chromium");
        assert_eq!(
            app_desktop_id("app-gnome-nm\\x2dapplet@autostart.service"),
            "gnome-nm\\x2dapplet"
        );
    }

    fn wm_probe(holds_session: bool) -> Fake {
        let mut f = Fake::default();
        f.put(
            "/proc/812/cgroup",
            &v2("/user.slice/user-1000.slice/user@1000.service/session.slice/wayland-wm@hyprland.desktop.service"),
        );
        f.units.insert(
            "wayland-wm@hyprland.desktop.service".into(),
            UnitProps {
                unit_file_state: "enabled".into(),
                restart: "no".into(),
                main_pid: Some(812),
                holds_session,
                ..UnitProps::default()
            },
        );
        f
    }

    #[test]
    fn the_unit_that_holds_the_desktop_session_offers_no_way_to_stop_it() {
        let w = explain_with(&wm_probe(true), 812, 100, &table());
        let a = &w.actions;
        assert!(!a.stop && !a.restart && !a.disable);
        assert_eq!(a.unit.as_deref(), Some("wayland-wm@hyprland.desktop.service"));
        assert_eq!(w.if_you_end.as_deref(), Some("Ending it ends your desktop session."));
    }

    #[test]
    fn a_bar_that_is_only_part_of_the_session_is_still_stoppable() {
        let w = explain_with(&wm_probe(false), 812, 100, &table());
        assert!(w.actions.stop && w.actions.restart && w.actions.disable);
        assert_eq!(w.if_you_end.as_deref(), Some("systemd does not start it again."));
    }

    #[test]
    fn a_unit_holds_the_session_only_when_stopping_it_stops_a_unit_that_binds_the_session() {
        let binds = |n: &str| match n {
            "wayland-session@hyprland.desktop.target" => vec!["graphical-session.target".to_owned()],
            _ => vec!["basic.target".to_owned()],
        };
        let units = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert!(holds_session(
            &units(&["wayland-session@hyprland.desktop.target"]),
            binds
        ));
        assert!(holds_session(&units(&["graphical-session.target"]), binds));
        assert!(!holds_session(&units(&["some.target"]), binds));
        assert!(!holds_session(&[], binds), "PartOf alone never reaches this list");
    }

    fn disabled_probe(active_enter: Option<u64>) -> Fake {
        let mut f = sshd_probe("no");
        let p = f.units.get_mut("sshd.service").unwrap();
        p.unit_file_state = "disabled".into();
        p.active_enter = active_enter;
        f
    }

    #[test]
    fn a_running_service_that_is_disabled_says_when_it_started_and_that_nothing_starts_it() {
        let w = explain_with(&disabled_probe(Some(1_000_000)), 812, 100, &table());
        assert_eq!(
            w.sentence,
            "System service OpenSSH Daemon (sshd.service), started at 09:12. It is not set to start automatically."
        );
        let w = explain_with(&disabled_probe(None), 812, 100, &table());
        assert_eq!(
            w.sentence,
            "System service OpenSSH Daemon (sshd.service). It is not set to start automatically."
        );
    }

    fn stat(pid: u32, sid: u32, start: u64) -> String {
        format!("{pid} (x) S 1 {sid} {sid} 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 {start} 0 0")
    }

    fn orphan_probe(leader: Option<(u32, u64)>) -> Fake {
        let mut f = Fake::default();
        f.put(
            "/proc/950/cgroup",
            &v2("/user.slice/user-1000.slice/user@1000.service/app.slice/run-r1a2b.scope"),
        );
        f.put("/proc/950/stat", &stat(950, 700, 500));
        if let Some((sid, start)) = leader {
            f.put(&format!("/proc/{sid}/stat"), &stat(sid, sid, start));
        }
        f
    }

    fn orphan_table() -> Vec<Process> {
        vec![row(1, 0, "systemd"), row(950, 1, "server")]
    }

    #[test]
    fn a_process_whose_session_leader_is_gone_is_an_orphan() {
        let w = explain_with(&orphan_probe(None), 950, 100, &orphan_table());
        assert_eq!(w.cause, Cause::Orphan);
    }

    #[test]
    fn a_process_whose_session_leader_still_runs_is_no_orphan() {
        let w = explain_with(&orphan_probe(Some((700, 400))), 950, 100, &orphan_table());
        assert!(matches!(w.cause, Cause::Unknown { .. }), "{:?}", w.cause);
    }

    #[test]
    fn a_session_leader_pid_reused_by_a_newer_process_does_not_count_as_running() {
        let w = explain_with(&orphan_probe(Some((700, 900))), 950, 100, &orphan_table());
        assert_eq!(w.cause, Cause::Orphan);
    }

    fn timer_probe(last: Option<u64>, next: Option<u64>) -> Fake {
        let mut f = sshd_probe("no");
        f.units.get_mut("sshd.service").unwrap().triggered_by = vec!["man-db.timer".into()];
        f.units.insert(
            "man-db.timer".into(),
            UnitProps {
                last_trigger: last,
                next_elapse: next,
                ..UnitProps::default()
            },
        );
        f
    }

    #[test]
    fn a_timer_sentence_gives_the_last_start_and_the_next_run_when_known() {
        let at = Some(1_000_000);
        let said = |l, n| explain_with(&timer_probe(l, n), 812, 100, &table()).sentence;
        assert_eq!(
            said(at, at),
            "Started by the timer man-db.timer at 09:12; next run 03:00 tomorrow."
        );
        assert_eq!(said(at, None), "Started by the timer man-db.timer at 09:12.");
        assert_eq!(
            said(None, at),
            "Started by the timer man-db.timer next run 03:00 tomorrow."
        );
        assert_eq!(said(None, None), "Started by the timer man-db.timer.");
    }

    #[test]
    fn the_next_run_names_the_day_only_when_it_is_not_today_or_tomorrow() {
        let tz = jiff::tz::TimeZone::UTC;
        let at = |s: &str| s.parse::<jiff::Timestamp>().unwrap().to_zoned(tz.clone());
        let now = at("2026-10-22T18:00:00Z");
        assert_eq!(next_phrase(&at("2026-10-22T23:30:00Z"), &now), "at 23:30");
        assert_eq!(next_phrase(&at("2026-10-23T03:00:00Z"), &now), "03:00 tomorrow");
        assert_eq!(next_phrase(&at("2026-10-25T03:00:00Z"), &now), "on Oct 25 at 03:00");
    }

    #[test]
    fn a_program_replaced_since_it_started_says_so_and_an_unreadable_exe_says_nothing() {
        let mut f = sshd_probe("no");
        assert!(explain_with(&f, 812, 100, &table()).facts.is_empty());
        f.put("/proc/812/exe", "/usr/bin/sshd");
        assert!(explain_with(&f, 812, 100, &table()).facts.is_empty());
        f.put("/proc/812/exe", "/usr/bin/sshd (deleted)");
        assert_eq!(
            explain_with(&f, 812, 100, &table()).facts,
            ["Running an older copy of this program; restart it to use the installed one."]
        );
    }

    #[test]
    fn live_reads_proc_files_from_its_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("7")).unwrap();
        std::fs::write(dir.path().join("7/cgroup"), "0::/system.slice/a.service\n").unwrap();
        let live = Live {
            proc_root: dir.path().into(),
        };
        assert_eq!(live.proc_file(7, "cgroup").unwrap(), "0::/system.slice/a.service\n");
        assert!(live.proc_file(8, "cgroup").is_err());
    }

    fn origin(cgroup: &str, chain: &[(u32, &str)]) -> Origin {
        Origin {
            cgroup: cgroup.into(),
            chain: chain.iter().map(|(p, c)| (*p, (*c).into())).collect(),
        }
    }

    #[test]
    fn a_recorded_service_whose_unit_is_gone_names_only_the_unit() {
        let o = origin("/system.slice/gone.service", &[(812, "gone"), (1, "systemd")]);
        let w = explain_recorded_with(&Fake::default(), &o, 100);
        assert_eq!(w.sentence, "System service gone.service.");
        assert!(w.if_you_end.is_none() && w.actions == Actions::default());
    }

    #[test]
    fn a_recorded_service_whose_unit_still_exists_says_the_details_are_as_of_now() {
        let o = origin("/system.slice/sshd.service", &[(812, "sshd"), (1, "systemd")]);
        let w = explain_recorded_with(&sshd_probe("on-failure"), &o, 100);
        assert_eq!(
            w.sentence,
            "System service OpenSSH Daemon (sshd.service), enabled at boot (as of now)."
        );
        assert!(w.if_you_end.is_none(), "an exited process has nothing to end");
    }

    #[test]
    fn a_recorded_process_is_classified_from_its_recorded_parents() {
        let o = origin("/", &[(900, "make"), (800, "bash"), (700, "kitty"), (1, "systemd")]);
        let w = explain_recorded_with(&Fake::default(), &o, 100);
        assert!(
            matches!(w.cause, Cause::Terminal { emulator: true, .. }),
            "{:?}",
            w.cause
        );
    }
}
