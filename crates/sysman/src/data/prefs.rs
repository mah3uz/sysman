use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Small UI preferences (column choices, sidebar widths) kept in
/// `$XDG_CONFIG_HOME/sysman/ui.toml`, one table per key.
pub struct Prefs {
    path: PathBuf,
    values: BTreeMap<String, toml::Value>,
}

impl Prefs {
    pub fn load(path: PathBuf) -> Self {
        let values = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| toml::from_str::<BTreeMap<String, toml::Value>>(&text).ok())
            .unwrap_or_default();
        Self { path, values }
    }

    /// The stored value, or `None` when absent or no longer the expected shape.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.values.get(key)?.clone().try_into().ok()
    }

    pub fn set<T: Serialize>(&mut self, key: &str, value: &T) {
        let Ok(value) = toml::Value::try_from(value) else {
            return;
        };
        if self.values.get(key) == Some(&value) {
            return;
        }
        self.values.insert(key.to_string(), value);
        if let Err(e) = self.save() {
            eprintln!("sysman: could not save {}: {e}", self.path.display());
        }
    }

    fn save(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string(&self.values).map_err(std::io::Error::other)?;
        // a crash mid-write must not leave a truncated file behind
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, &self.path)
    }
}

fn default_path() -> PathBuf {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if Path::new(&dir).is_absolute() => PathBuf::from(dir),
        _ => std::env::home_dir().unwrap_or_default().join(".config"),
    };
    base.join("sysman/ui.toml")
}

/// The process-wide preferences, loaded on first use.
pub fn prefs() -> &'static Mutex<Prefs> {
    static PREFS: OnceLock<Mutex<Prefs>> = OnceLock::new();
    PREFS.get_or_init(|| Mutex::new(Prefs::load(default_path())))
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Sidebar {
        width: f32,
        collapsed: bool,
    }

    #[test]
    fn values_survive_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        let mut p = Prefs::load(path.clone());
        p.set(
            "explorer.left",
            &Sidebar {
                width: 280.,
                collapsed: false,
            },
        );
        p.set("processes.columns", &vec!["pid", "user"]);
        let p = Prefs::load(path);
        assert_eq!(
            p.get::<Sidebar>("explorer.left"),
            Some(Sidebar {
                width: 280.,
                collapsed: false
            })
        );
        assert_eq!(p.get::<Vec<String>>("processes.columns").unwrap(), ["pid", "user"]);
    }

    #[test]
    fn a_broken_or_reshaped_value_falls_back_to_the_default() {
        // a hand-edited or older file must not stop the app; callers use their default instead
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        std::fs::write(&path, "[\"explorer.left\"]\nwidth = \"wide\"\n").unwrap();
        assert_eq!(Prefs::load(path.clone()).get::<Sidebar>("explorer.left"), None);
        std::fs::write(&path, "not toml at all [[[").unwrap();
        assert_eq!(Prefs::load(path).get::<Sidebar>("explorer.left"), None);
    }
}
