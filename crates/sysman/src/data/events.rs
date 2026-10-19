use std::cmp::Reverse;

use anyhow::Result;
use gpui::{Hsla, SharedString};
use gpui_kit_assets::IconName;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

use crate::data::history::History;
use crate::ui::theme::Theme;

/// Declared most severe first, so ordering by kind ranks events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Crash,
    PowerLoss,
    Oom,
    RestartLoop,
    UnitFailed,
    Packages,
    Boot,
    Shutdown,
    Suspend,
    Resume,
}

impl Kind {
    fn parse(kind: &str, data: &Value) -> Option<Self> {
        Some(match kind {
            "crash" => Self::Crash,
            "oom" => Self::Oom,
            "unit_failed" => Self::UnitFailed,
            "restart_loop" => Self::RestartLoop,
            "packages" => Self::Packages,
            "boot" => Self::Boot,
            "boot_end" if data["clean"].as_bool() == Some(false) => Self::PowerLoss,
            "boot_end" => Self::Shutdown,
            "suspend" => Self::Suspend,
            "resume" => Self::Resume,
            _ => return None,
        })
    }

    /// Worth a row in Recent events; boots and sleep only mark the charts.
    pub fn notable(self) -> bool {
        self <= Self::Packages
    }

    pub fn icon(self) -> IconName {
        match self {
            Self::Crash => IconName::Skull,
            Self::PowerLoss => IconName::Zap,
            Self::Oom => IconName::MemoryStick,
            Self::RestartLoop => IconName::RefreshCw,
            Self::UnitFailed => IconName::CircleX,
            Self::Packages => IconName::Blocks,
            Self::Boot => IconName::Play,
            Self::Shutdown | Self::Suspend | Self::Resume => IconName::Pause,
        }
    }

    pub fn color(self, t: &Theme) -> Hsla {
        match self {
            Self::Crash | Self::PowerLoss | Self::Oom => t.red,
            Self::RestartLoop | Self::UnitFailed => t.orange,
            Self::Packages => t.blue,
            _ => t.text_secondary,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub t: i64,
    pub kind: Kind,
    pub summary: SharedString,
}

impl Event {
    pub fn t_us(&self) -> u64 {
        self.t.max(0) as u64 * 1_000_000
    }
}

/// Events in `[from, to)`, oldest first; `None` when the collector predates the `events` table.
pub fn between(history: &History, from: i64, to: i64) -> Result<Option<Vec<Event>>> {
    let conn = history.conn();
    let exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'events'",
            [],
            |_| Ok(()),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(None);
    }
    let mut stmt =
        conn.prepare_cached("SELECT t, kind, summary, data FROM events WHERE t >= ?1 AND t < ?2 ORDER BY t, kind")?;
    let rows = stmt.query_map(params![from, to], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (t, kind, summary, data) = row?;
        let data = serde_json::from_str(&data).unwrap_or(Value::Null);
        if let Some(kind) = Kind::parse(&kind, &data) {
            out.push(Event {
                t,
                kind,
                summary: summary.into(),
            });
        }
    }
    Ok(Some(out))
}

/// The `n` notable events most worth a look: most severe first, newest first within a kind.
pub fn worst(events: &[Event], n: usize) -> Vec<&Event> {
    let mut notable: Vec<&Event> = events.iter().filter(|e| e.kind.notable()).collect();
    notable.sort_by_key(|e| (e.kind, Reverse(e.t)));
    notable.truncate(n);
    notable
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    fn history(schema: &str) -> (tempfile::TempDir, History) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        Connection::open(&path).unwrap().execute_batch(schema).unwrap();
        let history = History::open(&path).unwrap();
        (dir, history)
    }

    #[test]
    fn a_database_from_before_events_reads_as_unavailable_not_as_an_error() {
        let (_dir, h) = history("CREATE TABLE series (id INTEGER PRIMARY KEY, path TEXT);");
        assert_eq!(between(&h, 0, 100).unwrap(), None);
    }

    #[test]
    fn rows_in_the_window_come_back_typed_with_how_the_boot_ended() {
        let rows = r#"INSERT INTO events VALUES
            (10, 'boot_end', 'a', 'a', 'Boot ended without a shutdown', '{"clean": false}'),
            (20, 'boot_end', 'b', 'b', 'Shut down', '{"clean": true}'),
            (30, 'crash', 'c', 'b', 'foo crashed (SIGSEGV)', '{"comm": "foo"}'),
            (40, 'someday', 'd', 'b', 'A kind this build does not know', '{}'),
            (100, 'oom', 'e', 'b', 'Out of memory', '{}');"#;
        let (_dir, h) = history(&format!("{}{rows}", schema::db::SCHEMA));
        let got = between(&h, 0, 100).unwrap().unwrap();
        let kinds: Vec<_> = got.iter().map(|e| (e.t, e.kind)).collect();
        assert_eq!(
            kinds,
            [(10, Kind::PowerLoss), (20, Kind::Shutdown), (30, Kind::Crash)],
            "an unknown kind is skipped and `to` is exclusive"
        );
        assert_eq!(got[2].t_us(), 30_000_000, "Logs opens at microseconds");
    }

    #[test]
    fn the_worst_list_ranks_by_severity_and_leaves_out_boots_and_sleep() {
        let e = |t, kind| Event {
            t,
            kind,
            summary: SharedString::default(),
        };
        let events = [
            e(1, Kind::Packages),
            e(2, Kind::Boot),
            e(3, Kind::UnitFailed),
            e(4, Kind::Crash),
            e(5, Kind::Suspend),
            e(6, Kind::Crash),
            e(7, Kind::PowerLoss),
        ];
        let got: Vec<_> = worst(&events, 4).iter().map(|e| (e.t, e.kind)).collect();
        assert_eq!(
            got,
            [
                (6, Kind::Crash),
                (4, Kind::Crash),
                (7, Kind::PowerLoss),
                (3, Kind::UnitFailed)
            ],
            "a crash outranks a newer power loss; packages fall off the end"
        );
    }
}
