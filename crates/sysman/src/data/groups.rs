use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use schema::processes::Process;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Colour {
    #[default]
    Blue,
    Green,
    Orange,
    Red,
    Teal,
    Purple,
    Pink,
    Indigo,
}

impl Colour {
    pub const ALL: [Colour; 8] = [
        Self::Blue,
        Self::Green,
        Self::Orange,
        Self::Red,
        Self::Teal,
        Self::Purple,
        Self::Pink,
        Self::Indigo,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Field {
    /// The kernel's command name, at most 15 characters.
    Comm,
    Cmdline,
    User,
}

impl Field {
    pub const ALL: [Field; 3] = [Self::Comm, Self::Cmdline, Self::User];

    pub fn label(self) -> &'static str {
        match self {
            Self::Comm => "Command name",
            Self::Cmdline => "Command line",
            Self::User => "User",
        }
    }

    pub fn placeholder(self) -> &'static str {
        match self {
            Self::Comm => "firefox",
            Self::Cmdline => "/usr/lib/firefox",
            Self::User => "root",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Is,
    Contains,
    StartsWith,
}

impl Op {
    pub const ALL: [Op; 3] = [Self::Is, Self::Contains, Self::StartsWith];

    pub fn label(self) -> &'static str {
        match self {
            Self::Is => "is",
            Self::Contains => "contains",
            Self::StartsWith => "starts with",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Condition {
    pub field: Field,
    pub op: Op,
    pub value: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not: bool,
}

impl Condition {
    pub fn new(field: Field, op: Op, value: &str) -> Self {
        Self {
            field,
            op,
            value: value.into(),
            not: false,
        }
    }

    /// Case-insensitive; an unknown user never matches, negated or not.
    fn test(&self, comm: &str, cmdline: &str, user: Option<&str>) -> bool {
        let subject = match self.field {
            Field::Comm => comm,
            Field::Cmdline => cmdline,
            Field::User => match user {
                Some(user) => user,
                None => return false,
            },
        };
        let subject = subject.to_lowercase();
        let value = self.value.trim().to_lowercase();
        let hit = match self.op {
            Op::Is => subject == value,
            Op::Contains => subject.contains(&value),
            Op::StartsWith => subject.starts_with(&value),
        };
        hit != self.not
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Combine {
    #[default]
    Any,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    #[serde(default)]
    pub colour: Colour,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(rename = "match", default)]
    pub combine: Combine,
    #[serde(rename = "rule", default)]
    pub rules: Vec<Condition>,
}

fn enabled() -> bool {
    true
}

impl Group {
    pub fn new(name: &str, colour: Colour, rules: Vec<Condition>) -> Self {
        Self {
            name: name.into(),
            colour,
            enabled: true,
            combine: Combine::Any,
            rules,
        }
    }

    /// Conditions with a value; blank rows in the editor are ignored.
    fn conditions(&self) -> impl Iterator<Item = &Condition> {
        self.rules.iter().filter(|c| !c.value.trim().is_empty())
    }

    pub fn has_conditions(&self) -> bool {
        self.conditions().next().is_some()
    }

    /// A group without conditions matches nothing, so a fresh group is not "everything".
    pub fn matches(&self, comm: &str, cmdline: &str, user: Option<&str>) -> bool {
        let mut conditions = self.conditions().peekable();
        if conditions.peek().is_none() {
            return false;
        }
        match self.combine {
            Combine::Any => conditions.any(|c| c.test(comm, cmdline, user)),
            Combine::All => conditions.all(|c| c.test(comm, cmdline, user)),
        }
    }

    pub fn matches_process(&self, p: &Process) -> bool {
        self.matches(&p.comm, &p.cmdline, p.user.as_deref())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(rename = "group", default)]
    groups: Vec<Group>,
}

pub fn config_path() -> PathBuf {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if Path::new(&dir).is_absolute() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"),
    };
    base.join("sysman/groups.toml")
}

/// A missing file is no groups; an unreadable one is an error, so it is never overwritten by an empty list.
pub fn load(path: &Path) -> Result<Vec<Group>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let file: File = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(file.groups)
}

const HEADER: &str = "# Process groups for sysman. Edit here or in the Groups tab.\n\
# match = \"any\" or \"all\"; each [[group.rule]] has field (comm, cmdline, user),\n\
# op (is, contains, starts_with), value, and optionally not = true. Matching ignores case.\n\n";

/// Written to a temporary file beside the target and renamed over it, so a crash never leaves half a file.
pub fn save(path: &Path, groups: &[Group]) -> Result<()> {
    let dir = path.parent().context("config path has no directory")?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let body = toml::to_string_pretty(&File {
        groups: groups.to_vec(),
    })?;
    let tmp = dir.join(format!(".groups.toml.{}.tmp", std::process::id()));
    let written = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(HEADER.as_bytes())?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        fs::remove_file(&tmp).ok();
    }
    written.with_context(|| format!("writing {}", path.display()))
}

/// Common desktop app families, offered as one-click groups.
pub fn families() -> Vec<Group> {
    let any = |field, op, values: &[&str]| -> Vec<Condition> {
        values.iter().map(|v| Condition::new(field, op, v)).collect()
    };
    let mut browsers = any(Field::Cmdline, Op::Contains, &["firefox", "librewolf", "zen-browser"]);
    browsers.extend(any(
        Field::Comm,
        Op::Is,
        &["chrome", "chromium", "brave", "vivaldi-bin", "opera"],
    ));
    let mut editors = any(
        Field::Comm,
        Op::Is,
        &[
            "code",
            "codium",
            "cursor",
            "zed",
            "zed-editor",
            "nvim",
            "vim",
            "emacs",
            "hx",
            "kate",
            "gedit",
            "sublime_text",
        ],
    );
    editors.push(Condition::new(Field::Cmdline, Op::Contains, "jetbrains"));
    let chat = any(
        Field::Comm,
        Op::Contains,
        &[
            "slack",
            "discord",
            "telegram",
            "signal-desktop",
            "element",
            "teams",
            "vesktop",
            "whatsapp",
        ],
    );
    vec![
        Group::new("Browsers", Colour::Blue, browsers),
        Group::new("Editors", Colour::Purple, editors),
        Group::new("Chat", Colour::Green, chat),
    ]
}

/// A readable name for a systemd unit's cgroup path, e.g. `…/app-Hyprland-rustrover-f98cfc56.scope` → `rustrover`.
pub fn unit_name(cgroup: &str) -> String {
    let leaf = cgroup.rsplit('/').next().unwrap_or(cgroup).replace("\\x2d", "-");
    let mut name = leaf
        .strip_suffix(".scope")
        .or_else(|| leaf.strip_suffix(".service"))
        .unwrap_or(&leaf);
    if let Some((head, _)) = name.split_once('@') {
        name = head;
    }
    let mut parts: Vec<&str> = name.split('-').collect();
    // Scopes end in a random hex id; app units start with `app-` and often the launcher.
    while parts.len() > 1
        && parts
            .last()
            .is_some_and(|p| p.len() >= 6 && p.chars().all(|c| c.is_ascii_hexdigit()))
    {
        parts.pop();
    }
    if parts.len() > 1 && parts[0] == "app" {
        parts.remove(0);
        const LAUNCHERS: [&str; 8] = [
            "Hyprland", "hyprland", "gnome", "kde", "flatpak", "sway", "niri", "uwsm",
        ];
        if parts.len() > 1 && LAUNCHERS.contains(&parts[0]) {
            parts.remove(0);
        }
    }
    parts.join("-")
}

/// Distinct command names, heaviest process first.
pub fn heaviest_names<'a>(procs: impl IntoIterator<Item = &'a Process>, limit: usize) -> Vec<&'a str> {
    let mut procs: Vec<&Process> = procs.into_iter().collect();
    procs.sort_by_key(|p| std::cmp::Reverse(p.rss_bytes));
    let mut names: Vec<&str> = Vec::new();
    for p in procs {
        if names.len() == limit {
            break;
        }
        if !names.contains(&p.comm.as_str()) {
            names.push(&p.comm);
        }
    }
    names
}

/// Command names of `pids`' processes, heaviest first, as an any-of rule. A name that also runs outside
/// `pids` (a shell, `node`) is left out, or the preset would sweep in unrelated processes.
pub fn rules_for(pids: &[u32], table: &[Process], limit: usize) -> Vec<Condition> {
    let shared = |name: &str| table.iter().any(|p| p.comm == name && !pids.contains(&p.pid));
    heaviest_names(
        table.iter().filter(|p| pids.contains(&p.pid) && !shared(&p.comm)),
        limit,
    )
    .into_iter()
    .map(|n| Condition::new(Field::Comm, Op::Is, n))
    .collect()
}

pub fn cgroup_pids(cgroup: &str) -> Vec<u32> {
    fs::read_to_string(Path::new("/sys/fs/cgroup").join(cgroup).join("cgroup.procs"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// Live totals of the processes a group matches.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Totals {
    pub count: usize,
    pub rss: u64,
    pub cpu: f64,
}

pub fn totals<'a>(group: &Group, table: impl IntoIterator<Item = &'a Process>) -> Totals {
    table
        .into_iter()
        .filter(|p| group.matches_process(p))
        .fold(Totals::default(), |t, p| Totals {
            count: t.count + 1,
            rss: t.rss + p.rss_bytes,
            cpu: t.cpu + p.cpu_pct,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, comm: &str, cmdline: &str, user: Option<&str>, rss: u64, cpu: f64) -> Process {
        Process {
            pid,
            ppid: 1,
            comm: comm.into(),
            state: "S".into(),
            threads: 1,
            rss_bytes: rss,
            cpu_pct: cpu,
            read_bps: None,
            write_bps: None,
            user: user.map(Into::into),
            fds: None,
            net_rx_bps: None,
            net_tx_bps: None,
            cmdline: cmdline.into(),
            start_time: 0,
            ..Default::default()
        }
    }

    #[test]
    fn save_then_load_returns_the_same_groups_and_a_readable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sysman/groups.toml");
        let mut security = Group::new(
            "Security",
            Colour::Red,
            vec![
                Condition::new(Field::User, Op::Is, "root"),
                Condition {
                    not: true,
                    ..Condition::new(Field::Cmdline, Op::StartsWith, "/usr/bin/sshd")
                },
            ],
        );
        security.combine = Combine::All;
        security.enabled = false;
        let groups = vec![families().remove(0), security];
        save(&path, &groups).unwrap();
        assert_eq!(load(&path).unwrap(), groups);

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Process groups"), "the file explains itself");
        assert!(text.contains("[[group]]") && text.contains("[[group.rule]]"));
        assert!(text.contains("op = \"starts_with\"") && text.contains("match = \"all\""));
        assert_eq!(
            fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "no temporary file is left behind"
        );
    }

    #[test]
    fn a_hand_written_file_needs_only_names_and_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("groups.toml");
        fs::write(
            &path,
            "[[group]]\nname = \"Mine\"\n[[group.rule]]\nfield = \"comm\"\nop = \"is\"\nvalue = \"x\"\n",
        )
        .unwrap();
        let groups = load(&path).unwrap();
        assert_eq!(groups[0].colour, Colour::Blue);
        assert!(groups[0].enabled, "a group is enabled unless the file says otherwise");
        assert_eq!(groups[0].combine, Combine::Any);
    }

    #[test]
    fn a_missing_file_is_empty_but_a_broken_one_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("groups.toml");
        assert!(load(&path).unwrap().is_empty());
        fs::write(&path, "[[group]]\nname = ").unwrap();
        assert!(
            load(&path).is_err(),
            "a typo must not read as no groups, or the next save would erase the user's file"
        );
    }

    #[test]
    fn conditions_match_by_field_and_operator_ignoring_case() {
        let firefox = proc(
            1,
            "Isolated Web Co",
            "/usr/lib/firefox/firefox -contentproc",
            Some("me"),
            1,
            0.,
        );
        let rule = |field, op, value| Group::new("g", Colour::Blue, vec![Condition::new(field, op, value)]);
        assert!(rule(Field::Comm, Op::Is, "isolated web co").matches_process(&firefox));
        assert!(!rule(Field::Comm, Op::Is, "isolated").matches_process(&firefox));
        assert!(rule(Field::Comm, Op::StartsWith, "Isolated").matches_process(&firefox));
        assert!(
            rule(Field::Cmdline, Op::Contains, "FIREFOX").matches_process(&firefox),
            "content processes are caught by their command line"
        );
        assert!(rule(Field::User, Op::Is, "me").matches_process(&firefox));
        assert!(!rule(Field::User, Op::Is, "root").matches_process(&firefox));
    }

    #[test]
    fn any_all_and_not_combine_conditions() {
        let root_sshd = proc(1, "sshd", "/usr/bin/sshd -D", Some("root"), 1, 0.);
        let root_cron = proc(2, "crond", "/usr/bin/crond", Some("root"), 1, 0.);
        let mine = proc(3, "sshd", "sshd: me", Some("me"), 1, 0.);
        let mut g = Group::new(
            "root but not sshd",
            Colour::Red,
            vec![
                Condition::new(Field::User, Op::Is, "root"),
                Condition {
                    not: true,
                    ..Condition::new(Field::Comm, Op::Is, "sshd")
                },
            ],
        );
        g.combine = Combine::All;
        assert!(!g.matches_process(&root_sshd));
        assert!(g.matches_process(&root_cron));
        assert!(!g.matches_process(&mine));
        g.combine = Combine::Any;
        assert!(g.matches_process(&root_sshd) && g.matches_process(&root_cron));
        assert!(!g.matches_process(&mine), "an sshd owned by me fails both conditions");
    }

    #[test]
    fn a_group_without_conditions_matches_nothing() {
        let p = proc(1, "bash", "bash", Some("me"), 1, 0.);
        let blank = Group::new(
            "New group",
            Colour::Blue,
            vec![Condition::new(Field::Comm, Op::Is, "  ")],
        );
        assert!(
            !blank.matches_process(&p),
            "a negated blank row would otherwise match everything"
        );
        let unknown_owner = proc(2, "kworker", "", None, 1, 0.);
        let not_root = Group::new(
            "g",
            Colour::Blue,
            vec![Condition {
                not: true,
                ..Condition::new(Field::User, Op::Is, "root")
            }],
        );
        assert!(
            !not_root.matches_process(&unknown_owner),
            "an unreadable owner is not evidence"
        );
    }

    #[test]
    fn totals_sum_every_matching_instance() {
        let table = [
            proc(1, "chrome", "chrome", Some("me"), 100, 1.5),
            proc(2, "chrome", "chrome --type=renderer", Some("me"), 250, 20.),
            proc(3, "bash", "bash", Some("me"), 1000, 0.),
        ];
        let g = Group::new(
            "Chrome",
            Colour::Blue,
            vec![Condition::new(Field::Comm, Op::Is, "chrome")],
        );
        assert_eq!(
            totals(&g, &table),
            Totals {
                count: 2,
                rss: 350,
                cpu: 21.5
            }
        );
    }

    #[test]
    fn unit_names_drop_scope_ids_app_prefixes_and_launchers() {
        let app = "user.slice/user-1000.slice/user@1000.service/app.slice/app-graphical.slice/";
        assert_eq!(
            unit_name(&format!("{app}app-Hyprland-rustrover-f98cfc56.scope")),
            "rustrover"
        );
        assert_eq!(unit_name(&format!("{app}app-DashBeam@autostart.service")), "DashBeam");
        assert_eq!(unit_name("system.slice/tailscaled.service"), "tailscaled");
        assert_eq!(unit_name("system.slice/docker-cd29ba772c204c26.scope"), "docker");
        assert_eq!(unit_name("x/demo-daemon-903686bd6b2def88.scope"), "demo-daemon");
        assert_eq!(unit_name("x/wayland-wm@hyprland.desktop.service"), "wayland-wm");
        assert_eq!(
            unit_name("x/app-gnome-org.gnome.Nautilus-1234567.scope"),
            "org.gnome.Nautilus"
        );
    }

    #[test]
    fn cgroup_presets_name_its_processes_heaviest_first() {
        let table = [
            proc(10, "small", "", None, 1, 0.),
            proc(11, "big", "", None, 9, 0.),
            proc(12, "big", "", None, 5, 0.),
            proc(13, "outside", "", None, 99, 0.),
            proc(14, "bash", "", None, 50, 0.),
            proc(15, "bash", "", None, 50, 0.),
        ];
        let unit = [10, 11, 12, 14];
        let rules = rules_for(&unit, &table, 8);
        let names: Vec<&str> = rules.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(
            names,
            ["big", "small"],
            "bash also runs outside the unit, so it is left out"
        );
        assert_eq!(rules_for(&unit, &table, 1).len(), 1);
        let preset = Group::new("unit", Colour::Teal, rules);
        assert_eq!(
            totals(&preset, &table).count,
            3,
            "the preset catches exactly the unit's own processes"
        );
    }
}
