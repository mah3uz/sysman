use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use jiff::civil::Date;
use serde_json::Value;

/// Daily `YYYY-MM-DD.jsonl` files, each line forced to disk so the last
/// reading before a crash survives.
pub struct Jsonl {
    dir: PathBuf,
    fsync: bool,
    keep_days: u32,
    day: Option<Date>,
    file: Option<File>,
}

impl Jsonl {
    pub fn new(dir: PathBuf, fsync: bool, keep_days: u32) -> Result<Self> {
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            dir,
            fsync,
            keep_days,
            day: None,
            file: None,
        })
    }

    pub fn day(&self) -> Option<Date> {
        self.day
    }

    /// Switch to the file for `day` and delete files that fell out of retention.
    pub fn open(&mut self, day: Date) -> Result<()> {
        let path = self.dir.join(format!("{day}.jsonl"));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        self.file = Some(file);
        self.day = Some(day);
        self.prune(day);
        Ok(())
    }

    pub fn write(&mut self, record: &Value) -> Result<()> {
        let file = self.file.as_mut().context("no log file open")?;
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        file.write_all(&line)?;
        if self.fsync {
            file.sync_data()?;
        }
        Ok(())
    }

    /// Keeps `keep_days` days counting today; ignores files it did not name.
    fn prune(&self, today: Date) {
        let Ok(oldest) = today.checked_sub(jiff::Span::new().days(i64::from(self.keep_days) - 1)) else {
            return;
        };
        for entry in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let Some(date) = name
                .to_str()
                .and_then(|n| n.strip_suffix(".jsonl"))
                .and_then(|d| d.parse::<Date>().ok())
            else {
                continue;
            };
            if date < oldest {
                if let Err(e) = std::fs::remove_file(entry.path()) {
                    eprintln!("could not delete old log {}: {e}", entry.path().display());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;
    use serde_json::json;

    use super::*;

    #[test]
    fn appends_one_line_per_record_across_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Jsonl::new(dir.path().into(), true, 2).unwrap();
        log.open(date(2026, 10, 15)).unwrap();
        log.write(&json!({"a": 1})).unwrap();
        // a restart on the same day appends instead of truncating the evidence
        let mut log = Jsonl::new(dir.path().into(), true, 2).unwrap();
        log.open(date(2026, 10, 15)).unwrap();
        log.write(&json!({"a": 2})).unwrap();
        let text = std::fs::read_to_string(dir.path().join("2026-10-15.jsonl")).unwrap();
        assert_eq!(text, "{\"a\":1}\n{\"a\":2}\n");
    }

    #[test]
    fn keeps_today_and_yesterday_and_leaves_foreign_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "2026-10-12.jsonl",
            "2026-10-13.jsonl",
            "2026-10-14.jsonl",
            "notes.txt",
            "2026-10-01.jsonl.gz",
        ] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let mut log = Jsonl::new(dir.path().into(), false, 2).unwrap();
        log.open(date(2026, 10, 15)).unwrap();
        let mut left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                "2026-10-01.jsonl.gz",
                "2026-10-14.jsonl",
                "2026-10-15.jsonl",
                "notes.txt"
            ]
        );
    }
}
