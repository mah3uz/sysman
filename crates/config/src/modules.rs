use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Parse a module's own options, rejecting keys it does not know.
pub fn options<T: DeserializeOwned>(table: toml::Table) -> Result<T> {
    toml::Value::Table(table).try_into().context("invalid module options")
}

/// For modules without options; any key is a typo.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoOptions {}

/// A collector as the config file sees it: its name, its interval, and the options it accepts.
pub struct Module {
    pub name: &'static str,
    pub default_interval: u64,
    defaults: fn() -> toml::Table,
    check: fn(toml::Table) -> Result<()>,
}

impl Module {
    /// Every option with its default value; the value's TOML type is the option's type.
    pub fn defaults(&self) -> toml::Table {
        (self.defaults)()
    }

    /// The daemon's own parse of these options.
    pub fn check(&self, options: toml::Table) -> Result<()> {
        (self.check)(options)
    }
}

const fn module<T: Default + Serialize + DeserializeOwned>(name: &'static str, default_interval: u64) -> Module {
    Module {
        name,
        default_interval,
        defaults: defaults::<T>,
        check: check::<T>,
    }
}

fn defaults<T: Default + Serialize>() -> toml::Table {
    toml::Table::try_from(T::default()).unwrap_or_default()
}

fn check<T: DeserializeOwned>(table: toml::Table) -> Result<()> {
    options::<T>(table).map(drop)
}

pub const MODULES: &[Module] = &[
    module::<cgroups::Options>("cgroups", 10),
    module::<cpu::Options>("cpu", 2),
    module::<disk::Options>("disk", 2),
    module::<fs::Options>("fs", 30),
    module::<gpu::Options>("gpu", 2),
    module::<NoOptions>("helper", 10),
    module::<hwmon::Options>("hwmon", 2),
    module::<NoOptions>("memory", 2),
    module::<net::Options>("net", 2),
    module::<netinfo::Options>("netinfo", 60),
    module::<NoOptions>("power", 60),
    module::<NoOptions>("pressure", 2),
    module::<processes::Options>("processes", 10),
    module::<storage::Options>("storage", 60),
    module::<NoOptions>("system", 2),
];

pub fn find(name: &str) -> Option<&'static Module> {
    MODULES.iter().find(|m| m.name == name)
}

pub mod cgroups {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields, default)]
    pub struct Options {
        pub top_n: usize,
    }

    impl Default for Options {
        fn default() -> Self {
            Self { top_n: 10 }
        }
    }
}

pub mod cpu {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields, default)]
    pub struct Options {
        pub per_core: bool,
        pub frequency: bool,
    }

    impl Default for Options {
        fn default() -> Self {
            Self {
                per_core: true,
                frequency: true,
            }
        }
    }
}

pub mod disk {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields, default)]
    pub struct Options {
        pub partitions: bool,
        pub exclude: Vec<String>,
    }

    impl Default for Options {
        fn default() -> Self {
            Self {
                partitions: false,
                exclude: vec!["loop".to_string()],
            }
        }
    }
}

pub mod fs {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields, default)]
    pub struct Options {
        pub exclude_fstypes: Vec<String>,
    }

    impl Default for Options {
        fn default() -> Self {
            Self {
                exclude_fstypes: vec!["squashfs".to_string(), "iso9660".to_string()],
            }
        }
    }
}

pub mod gpu {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields, default)]
    pub struct Options {
        /// Per-process usage (NVML or nvidia-smi, and DRM fdinfo).
        pub processes: bool,
    }

    impl Default for Options {
        fn default() -> Self {
            Self { processes: true }
        }
    }
}

pub mod hwmon {
    use std::collections::BTreeMap;

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields, default)]
    pub struct Options {
        /// Seconds between reads by chip-name prefix (longest match wins); unmatched chips are read every sample.
        pub chip_intervals: BTreeMap<String, u64>,
    }

    impl Default for Options {
        fn default() -> Self {
            // the nct67xx driver re-reads every register over port I/O when its cache is older than ~1.5 s
            Self {
                chip_intervals: BTreeMap::from([("nct67".to_string(), 10)]),
            }
        }
    }
}

pub mod net {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields, default)]
    pub struct Options {
        /// Interface name prefixes to skip.
        pub exclude: Vec<String>,
    }

    impl Default for Options {
        fn default() -> Self {
            Self {
                exclude: vec!["lo".into(), "veth".into()],
            }
        }
    }
}

pub mod netinfo {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields, default)]
    pub struct Options {
        /// Interface name prefixes to skip.
        pub exclude: Vec<String>,
    }

    impl Default for Options {
        fn default() -> Self {
            Self {
                exclude: vec!["lo".into(), "veth".into()],
            }
        }
    }
}

pub mod processes {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields, default)]
    pub struct Options {
        pub top_n: usize,
        pub io: bool,
        pub groups: bool,
        pub cmdline_max: usize,
    }

    impl Default for Options {
        fn default() -> Self {
            Self {
                top_n: 10,
                io: true,
                groups: true,
                cmdline_max: 200,
            }
        }
    }
}

pub mod storage {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields, default)]
    pub struct Options {
        /// Block device name prefixes to skip.
        pub exclude: Vec<String>,
    }

    impl Default for Options {
        fn default() -> Self {
            Self {
                exclude: vec!["loop".into(), "zram".into()],
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_describe_each_option_by_its_type() {
        // the settings form picks its input from the default value's type
        let processes = find("processes").unwrap().defaults();
        assert_eq!(processes["top_n"], toml::Value::Integer(10));
        assert_eq!(processes["io"], toml::Value::Boolean(true));
        let storage = find("storage").unwrap().defaults();
        assert_eq!(storage["exclude"].as_array().unwrap().len(), 2);
        assert!(find("memory").unwrap().defaults().is_empty());
    }

    #[test]
    fn checks_use_the_daemons_strict_parse() {
        let cpu = find("cpu").unwrap();
        assert!(cpu.check(toml::from_str("per_core = false").unwrap()).is_ok());
        assert!(cpu.check(toml::from_str("per_cor = false").unwrap()).is_err(), "typo");
        assert!(
            cpu.check(toml::from_str("per_core = 1").unwrap()).is_err(),
            "wrong type"
        );
        assert!(find("memory").unwrap().check(toml::from_str("x = 1").unwrap()).is_err());
    }
}
