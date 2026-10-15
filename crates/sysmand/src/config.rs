use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    general: General,
    #[serde(default)]
    retention: Retention,
    #[serde(default)]
    modules: BTreeMap<String, toml::Table>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct General {
    pub interval: u64,
    pub fsync: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            interval: 2,
            fsync: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Retention {
    pub log_days: u32,
    pub raw_hours: u32,
    pub rollup_1m_days: u32,
    pub rollup_15m_days: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            log_days: 2,
            raw_hours: 48,
            rollup_1m_days: 90,
            rollup_15m_days: 730,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModuleConfig {
    pub enabled: bool,
    pub interval: Option<u64>,
    /// The module's own options, with `enabled` and `interval` removed.
    pub options: toml::Table,
}

#[derive(Debug)]
pub struct Config {
    pub general: General,
    pub retention: Retention,
    pub modules: BTreeMap<String, ModuleConfig>,
}

impl Config {
    /// A missing file means defaults; every other problem is an error.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: File = toml::from_str(text)?;
        if file.general.interval == 0 {
            bail!("[general] interval must be at least 1 second");
        }
        if file.retention.log_days == 0 {
            bail!("[retention] log_days must be at least 1 (today's file)");
        }
        let r = &file.retention;
        if r.raw_hours == 0 || r.rollup_1m_days == 0 || r.rollup_15m_days == 0 {
            bail!("[retention] raw_hours, rollup_1m_days and rollup_15m_days must be at least 1");
        }
        let mut modules = BTreeMap::new();
        for (name, mut table) in file.modules {
            let enabled = match table.remove("enabled") {
                None => true,
                Some(toml::Value::Boolean(b)) => b,
                Some(v) => bail!("[modules.{name}] enabled must be true or false, not {v}"),
            };
            let interval = match table.remove("interval") {
                None => None,
                Some(toml::Value::Integer(i)) if i >= 1 => Some(i as u64),
                Some(v) => bail!("[modules.{name}] interval must be a whole number of seconds >= 1, not {v}"),
            };
            modules.insert(
                name,
                ModuleConfig {
                    enabled,
                    interval,
                    options: table,
                },
            );
        }
        Ok(Self {
            general: file.general,
            retention: file.retention,
            modules,
        })
    }
}

pub fn config_path() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("sysman/config.toml")
}

pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("sysman")
}

/// `None` when `XDG_RUNTIME_DIR` is unset or not absolute.
pub fn socket_path() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    dir.is_absolute().then(|| dir.join("sysman.sock"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        // the spec says relative values are invalid and must be ignored
        Some(dir) if Path::new(&dir).is_absolute() => PathBuf::from(dir),
        _ => std::env::home_dir().unwrap_or_default().join(fallback),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.general.interval, 2);
        assert!(c.general.fsync, "crash evidence depends on fsync being on by default");
        assert!(c.modules.is_empty());
    }

    #[test]
    fn typos_are_rejected_not_ignored() {
        // a silently ignored key would leave a setting at its default without anyone noticing
        assert!(Config::parse("[general]\nintervall = 5").is_err());
        assert!(Config::parse("[genral]\ninterval = 5").is_err());
        assert!(Config::parse("[retention]\nlog_day = 5").is_err());
    }

    #[test]
    fn common_module_keys_are_split_from_options() {
        let c = Config::parse("[modules.cpu]\nenabled = false\ninterval = 10\nper_core = true").unwrap();
        let m = &c.modules["cpu"];
        assert!(!m.enabled);
        assert_eq!(m.interval, Some(10));
        assert_eq!(m.options.keys().collect::<Vec<_>>(), ["per_core"]);
    }

    #[test]
    fn fractional_or_zero_intervals_are_rejected() {
        // the database stores whole seconds
        assert!(Config::parse("[general]\ninterval = 0").is_err());
        assert!(Config::parse("[modules.cpu]\ninterval = 1.5").is_err());
        assert!(Config::parse("[modules.cpu]\ninterval = 0").is_err());
    }
}
