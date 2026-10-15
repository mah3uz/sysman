use std::io;
use std::path::{Path, PathBuf};
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

/// What [`Watched::poll`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polled {
    Unchanged,
    /// Changed elsewhere and taken in; nothing here was unsaved.
    Reloaded,
    /// Changed elsewhere while there are unsaved edits here; the user chooses between the two.
    Conflict,
}

/// A config file open for editing that follows edits made to it elsewhere.
#[derive(Debug)]
pub struct Watched {
    path: PathBuf,
    /// `Err` while the file on disk is not TOML.
    pub draft: Result<Draft, String>,
    stamp: Stamp,
    pub conflict: bool,
}

impl Watched {
    pub fn open(path: PathBuf) -> Self {
        let stamp = Stamp::of(&path);
        let draft = Draft::load(&path).map_err(|e| format!("{e:#}"));
        Self {
            path,
            draft,
            stamp,
            conflict: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn dirty(&self) -> bool {
        self.draft.as_ref().is_ok_and(Draft::dirty)
    }

    pub fn poll(&mut self) -> Polled {
        if self.conflict || Stamp::of(&self.path) == self.stamp {
            return Polled::Unchanged;
        }
        if self.dirty() {
            self.conflict = true;
            return Polled::Conflict;
        }
        self.reload();
        Polled::Reloaded
    }

    /// Drops the unsaved edits for the file as it is now.
    pub fn reload(&mut self) {
        *self = Self::open(std::mem::take(&mut self.path));
    }

    /// Keeps the unsaved edits; saving them replaces the file changed elsewhere.
    pub fn keep_mine(&mut self) {
        self.stamp = Stamp::of(&self.path);
        self.conflict = false;
    }

    pub fn save(&mut self) -> Result<()> {
        let Ok(draft) = &mut self.draft else {
            anyhow::bail!("the file on disk is not TOML; fix or remove it first");
        };
        draft.save(&self.path)?;
        self.stamp = Stamp::of(&self.path);
        self.conflict = false;
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

    /// Writes with a different length, so the change shows even within the mtime's resolution.
    fn edit_by_hand(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn a_hand_edit_is_taken_in_when_nothing_is_unsaved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        edit_by_hand(&path, FILE);
        let mut w = Watched::open(path.clone());
        assert_eq!(w.poll(), Polled::Unchanged);
        edit_by_hand(&path, "[general]\ninterval = 7\n");
        assert_eq!(w.poll(), Polled::Reloaded);
        assert_eq!(w.draft.as_ref().unwrap().check().unwrap().general.interval, 7);
        assert_eq!(w.poll(), Polled::Unchanged, "taken in once");
    }

    #[test]
    fn a_hand_edit_during_unsaved_edits_asks_and_destroys_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        edit_by_hand(&path, FILE);
        let mut w = Watched::open(path.clone());
        w.draft.as_mut().unwrap().set(&["general", "interval"], 4);
        edit_by_hand(&path, "[general]\ninterval = 7\n");
        assert_eq!(w.poll(), Polled::Conflict);
        assert!(w.dirty(), "the unsaved edit is still there");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[general]\ninterval = 7\n",
            "and so is theirs"
        );
        assert_eq!(w.poll(), Polled::Unchanged, "asked once, not on every poll");

        let mut keep = Watched::open(path.clone());
        keep.draft.as_mut().unwrap().set(&["general", "interval"], 4);
        edit_by_hand(&path, "[general]\ninterval = 9\n");
        assert_eq!(keep.poll(), Polled::Conflict);
        keep.keep_mine();
        keep.save().unwrap();
        assert_eq!(Config::load(&path).unwrap().general.interval, 4);

        w.reload();
        assert!(!w.dirty() && !w.conflict);
        assert_eq!(w.draft.as_ref().unwrap().check().unwrap().general.interval, 4);
    }

    #[test]
    fn our_own_save_is_not_an_outside_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sysman/config.toml");
        let mut w = Watched::open(path.clone());
        w.draft.as_mut().unwrap().set(&["general", "fsync"], false);
        w.save().unwrap();
        assert_eq!(w.poll(), Polled::Unchanged);
    }

    #[test]
    fn a_file_broken_by_hand_opens_as_an_error_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        edit_by_hand(&path, "[general\n");
        let mut w = Watched::open(path.clone());
        assert!(w.draft.is_err());
        assert!(w.save().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[general\n");
        edit_by_hand(&path, "[general]\n");
        assert_eq!(w.poll(), Polled::Reloaded);
        assert!(w.draft.is_ok());
    }
}
