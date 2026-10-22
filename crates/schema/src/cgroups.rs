use serde::{Deserialize, Serialize};

/// A leaf cgroup. `name` is the path under `/sys/fs/cgroup`; `cpu_pct` is percent of one core.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cgroup {
    pub name: String,
    pub cpu_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cgroups {
    pub count: u64,
    pub top_cpu: Vec<Cgroup>,
    pub top_memory: Vec<Cgroup>,
}

/// The systemd hierarchy's path: the `name=systemd` line of a v1 hybrid unless it is the root, else the v2 `0::`
/// line, else the first line's.
pub fn cgroup_path(text: &str) -> Option<&str> {
    let (mut systemd, mut v2, mut first) = (None, None, None);
    for line in text.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(_), Some(ctrl), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        match ctrl {
            "name=systemd" => systemd = Some(path),
            "" => v2 = Some(path),
            _ => {}
        }
        first.get_or_insert(path);
    }
    let path = systemd.filter(|p| *p != "/").or(v2).or(systemd).or(first)?;
    Some(path.trim_end_matches(" (deleted)"))
}

#[cfg(test)]
mod cgroup_path_tests {
    use super::cgroup_path;

    #[test]
    fn cgroup_path_is_the_systemd_hierarchys() {
        assert_eq!(
            cgroup_path("0::/system.slice/a.service"),
            Some("/system.slice/a.service")
        );
        let hybrid = "12:cpu,cpuacct:/\n1:name=systemd:/system.slice/cups.service\n0::/system.slice/cups.service";
        assert_eq!(cgroup_path(hybrid), Some("/system.slice/cups.service"));
        assert_eq!(
            cgroup_path("1:name=systemd:/\n0::/system.slice/cups.service"),
            Some("/system.slice/cups.service"),
            "a hybrid whose systemd line is the root names its unit on the v2 line"
        );
        assert_eq!(
            cgroup_path("4:memory:/user.slice\n1:name=systemd:/"),
            Some("/"),
            "pure v1: the systemd line, even at the root"
        );
        assert_eq!(cgroup_path("4:memory:/user.slice\n3:cpu:/x"), Some("/user.slice"));
        assert_eq!(cgroup_path("0::/a.slice/b.scope (deleted)"), Some("/a.slice/b.scope"));
        assert_eq!(cgroup_path(""), None);
    }
}
