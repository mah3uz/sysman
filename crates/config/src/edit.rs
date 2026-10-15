use std::io;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

use crate::Config;

/// What a file looked like when last read or written, to notice edits made elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp(Option<(SystemTime, u64)>);

impl Stamp {
    /// A missing file has a stamp too, so creating it counts as a change.
    pub fn of(path: &Path) -> Self {
        Self(
            std::fs::metadata(path)
                .ok()
                .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len())),
        )
    }
}

/// Writes through a temporary file, so a crash mid-write never leaves a truncated file behind.
pub fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// The collector config being edited: the user's file, comments and order kept, with unsaved changes on top.
#[derive(Debug, Clone)]
pub struct Draft {
    doc: DocumentMut,
    saved: String,
}

impl Draft {
    /// Fails only when the text is not TOML; a TOML file the daemon would reject still opens, to be fixed here.
    pub fn parse(text: &str) -> Result<Self> {
        Ok(Self {
            doc: text.parse().context("not a TOML file")?,
            saved: text.to_owned(),
        })
    }

    /// The draft as on disk; a missing file is an empty one.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn text(&self) -> String {
        self.doc.to_string()
    }

    pub fn dirty(&self) -> bool {
        self.text() != self.saved
    }

    pub fn get(&self, path: &[&str]) -> Option<&Item> {
        let (last, tables) = path.split_last()?;
        let mut table: &dyn TableLike = self.doc.as_table();
        for key in tables {
            table = table.get(key)?.as_table_like()?;
        }
        table.get(last)
    }

    /// Sets a value, creating its tables; a value already there keeps its comments.
    pub fn set(&mut self, path: &[&str], value: impl Into<Value>) {
        let Some((last, tables)) = path.split_last() else {
            return;
        };
        let mut value = value.into();
        let table = table_at(&mut self.doc, tables);
        // replaced in place: a comment above a key belongs to the key, which `insert` would replace too
        match table.get_mut(last).and_then(Item::as_value_mut) {
            Some(old) => {
                *value.decor_mut() = old.decor().clone();
                *old = value;
            }
            None => {
                table.insert(last, Item::Value(value));
            }
        }
    }

    /// Removes a key, so the daemon uses its default again.
    pub fn remove(&mut self, path: &[&str]) {
        let Some((last, tables)) = path.split_last() else {
            return;
        };
        let mut table: &mut dyn TableLike = self.doc.as_table_mut();
        for key in tables {
            match table.get_mut(key).and_then(Item::as_table_like_mut) {
                Some(t) => table = t,
                None => return,
            }
        }
        table.remove(last);
    }

    /// The daemon's own parse of the draft.
    pub fn check(&self) -> Result<Config> {
        Config::validate(&self.text())
    }

    /// Validates, then writes atomically; an invalid draft leaves the file untouched.
    pub fn save(&mut self, path: &Path) -> Result<()> {
        let text = self.text();
        Config::validate(&text)?;
        write_atomic(path, &text).with_context(|| format!("writing {}", path.display()))?;
        self.saved = text;
        Ok(())
    }
}

fn table_at<'a>(doc: &'a mut DocumentMut, path: &[&str]) -> &'a mut dyn TableLike {
    let mut table: &mut dyn TableLike = doc.as_table_mut();
    for (i, key) in path.iter().enumerate() {
        if !table.get(key).is_some_and(Item::is_table_like) {
            let mut new = Table::new();
            // `[modules.cpu]` needs no empty `[modules]` header above it
            new.set_implicit(i + 1 < path.len());
            table.insert(key, Item::Table(new));
        }
        table = table
            .get_mut(key)
            .and_then(Item::as_table_like_mut)
            .expect("inserted above");
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "# sysmand settings\n\n[general]\n# seconds between samples\ninterval = 2 # fast\nfsync = true\n\n[modules.cpu]\nper_core = true\n";

    #[test]
    fn edits_keep_the_users_comments_and_order() {
        let mut d = Draft::parse(FILE).unwrap();
        d.set(&["general", "interval"], 5);
        d.set(&["modules", "cpu", "enabled"], false);
        d.set(&["retention", "raw_hours"], 24);
        let text = d.text();
        assert!(
            text.starts_with("# sysmand settings\n\n[general]\n# seconds between samples\ninterval = 5 # fast\n"),
            "{text}"
        );
        assert!(
            text.contains("[modules.cpu]\nper_core = true\nenabled = false\n"),
            "{text}"
        );
        assert!(!text.contains("[modules]\n"), "no empty parent header: {text}");
        assert!(
            text.find("[general]") < text.find("[retention]"),
            "new tables go after the existing ones"
        );
        let c = d.check().unwrap();
        assert_eq!((c.general.interval, c.retention.raw_hours), (5, 24));
        assert!(!c.modules["cpu"].enabled);
    }

    #[test]
    fn an_untouched_draft_writes_back_byte_for_byte() {
        let d = Draft::parse(FILE).unwrap();
        assert_eq!(d.text(), FILE);
        assert!(!d.dirty());
    }

    #[test]
    fn removing_a_key_restores_the_default() {
        let mut d = Draft::parse(FILE).unwrap();
        d.remove(&["general", "interval"]);
        d.remove(&["modules", "gpu", "processes"]);
        assert!(d.dirty());
        assert_eq!(d.check().unwrap().general.interval, 2);
        assert!(d.text().contains("# sysmand settings"));
    }

    #[test]
    fn an_invalid_draft_is_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sysman/config.toml");
        write_atomic(&path, FILE).unwrap();
        let before = Stamp::of(&path);
        let mut d = Draft::load(&path).unwrap();
        d.set(&["general", "interval"], 0);
        assert!(d.save(&path).is_err(), "the daemon refuses a zero interval");
        d.set(&["general", "interval"], 3);
        d.set(&["modules", "cpu", "per_core"], "yes");
        assert!(d.save(&path).is_err(), "the daemon's option parse rejects a string");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), FILE);
        assert_eq!(Stamp::of(&path), before);
        assert!(d.dirty(), "the rejected edits stay in the form to be fixed");
    }

    #[test]
    fn saving_writes_the_file_and_clears_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sysman/config.toml");
        let mut d = Draft::load(&path).unwrap();
        assert_eq!(d.text(), "", "a missing file opens empty");
        d.set(
            &["modules", "storage", "exclude"],
            Value::Array(["loop", "zram", "sr"].into_iter().collect()),
        );
        d.save(&path).unwrap();
        assert!(!d.dirty());
        let c = Config::load(&path).unwrap();
        assert_eq!(c.modules["storage"].options["exclude"].as_array().unwrap().len(), 3);
        assert!(!dir.path().join("sysman/config.toml.tmp").exists());
    }

    #[test]
    fn a_stamp_notices_an_edit_made_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let missing = Stamp::of(&path);
        std::fs::write(&path, "[general]\n").unwrap();
        let created = Stamp::of(&path);
        assert_ne!(missing, created, "creating the file is a change");
        assert_eq!(Stamp::of(&path), created, "nothing changed");
        std::fs::write(&path, "[general]\ninterval = 5\n").unwrap();
        assert_ne!(
            Stamp::of(&path),
            created,
            "a longer file is a change even within the mtime's resolution"
        );
    }

    #[test]
    fn a_broken_file_does_not_open() {
        assert!(Draft::parse("[general\n").is_err());
        assert!(
            Draft::parse("[general]\nintervall = 5\n").is_ok(),
            "valid TOML opens so the form can show the error"
        );
    }
}
