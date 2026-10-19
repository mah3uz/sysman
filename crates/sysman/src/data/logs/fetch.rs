use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::Result;
use journal::Entry;
use journal::reader::{Fields, Journal, Seek};

use super::scan::{Scan, entry_hash};

const CACHE_ROWS: usize = 256;

/// The entries just before and after one, in journal order with no filter.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Around {
    /// Oldest first.
    pub before: Vec<Entry>,
    pub after: Vec<Entry>,
}

/// Reads whole entries by seeking, for the rows on screen and the inspector. It owns a journal handle and blocks
/// on journal I/O (roughly a millisecond a row), so use it from a background task and keep one per screen.
///
/// A row is turned back into its entry by seeking to its timestamp and taking, among the entries stamped exactly
/// then, the one with the row's hash that is the same number of identical ones in as the row is in the index
/// ([`Index::occurrence`](super::Index::occurrence)).
pub struct Fetcher {
    journal: Journal,
    scan: Arc<Scan>,
    cache: VecDeque<(u32, Arc<Entry>)>,
}

impl Fetcher {
    pub fn new(scan: Arc<Scan>) -> Result<Fetcher> {
        Ok(Fetcher {
            journal: super::open_journal()?,
            scan,
            cache: VecDeque::new(),
        })
    }

    /// The entry behind `row`, from a cache of the last 256 rows. `None` for a package row (use
    /// [`Index::transaction`](super::Index::transaction)) or when the entry has left the journal.
    pub fn entry(&mut self, row: u32) -> Result<Option<Arc<Entry>>> {
        if let Some(at) = self.cache.iter().position(|(r, _)| *r == row) {
            let hit = self.cache.remove(at).unwrap();
            self.cache.push_front(hit.clone());
            return Ok(Some(hit.1));
        }
        let (t_us, hash, nth) = {
            let index = self.scan.index();
            let Some(r) = index.row(row).filter(|r| !r.package) else {
                return Ok(None);
            };
            (r.t_us, r.hash, index.occurrence(row))
        };
        self.journal.set_filter(&Default::default())?;
        self.journal.seek(Seek::Realtime(t_us))?;
        let found = pick(|| self.journal.next(Fields::All), t_us, hash, nth)?;
        let Some(entry) = found.map(Arc::new) else {
            return Ok(None);
        };
        self.cache.push_front((row, entry.clone()));
        self.cache.truncate(CACHE_ROWS);
        Ok(Some(entry))
    }

    /// Up to `n` entries on each side of `row`'s entry, with no filter. Empty when the entry cannot be found.
    pub fn around(&mut self, row: u32, n: usize) -> Result<Around> {
        let Some(entry) = self.entry(row)? else {
            return Ok(Around::default());
        };
        let Some(cursor) = entry.get("__CURSOR") else {
            return Ok(Around::default());
        };
        let cursor = cursor.to_string();
        let mut out = Around::default();
        self.journal.seek(Seek::Cursor(cursor.clone()))?;
        for e in std::iter::from_fn(|| self.journal.next(Fields::All).transpose()).take(n + 1) {
            let e = e?;
            if e.get("__CURSOR") != Some(cursor.as_str()) {
                out.after.push(e);
            }
        }
        self.journal.seek(Seek::Cursor(cursor.clone()))?;
        for e in std::iter::from_fn(|| self.journal.previous(Fields::All).transpose()).take(n + 1) {
            let e = e?;
            if e.get("__CURSOR") != Some(cursor.as_str()) {
                out.before.push(e);
            }
        }
        out.after.truncate(n);
        out.before.truncate(n);
        out.before.reverse();
        Ok(out)
    }
}

/// From a stream positioned at `t_us`, the `nth` entry stamped `t_us` whose hash is `hash`.
fn pick(mut next: impl FnMut() -> Result<Option<Entry>>, t_us: u64, hash: u64, nth: usize) -> Result<Option<Entry>> {
    let mut seen = 0;
    while let Some(e) = next()? {
        if e.t_us != t_us {
            if e.t_us > t_us {
                break;
            }
            continue;
        }
        if entry_hash(&e) == hash {
            if seen == nth {
                return Ok(Some(e));
            }
            seen += 1;
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::super::index::tests::{build, entry};
    use super::*;

    fn tagged(t: u64, msg: &str, pid: &str) -> Entry {
        entry(t, 6, "a", msg, &[("_PID", pid)])
    }

    #[test]
    fn among_equal_timestamps_each_row_gets_its_own_entry() {
        let entries = [
            tagged(5, "x", "1"),
            tagged(5, "y", "2"),
            tagged(5, "x", "3"),
            tagged(6, "x", "4"),
        ];
        let idx = build(&entries);
        for (id, e) in entries.iter().enumerate() {
            let row = idx.row(id as u32).unwrap();
            let mut stream = entries.iter().cloned();
            let nth = idx.occurrence(id as u32);
            let got = pick(|| Ok(stream.next()), row.t_us, row.hash, nth).unwrap().unwrap();
            assert_eq!(got.get("_PID"), e.get("_PID"), "row {id}");
        }
    }

    #[test]
    fn an_entry_that_has_left_the_journal_is_not_replaced_by_a_neighbour() {
        let entries = [tagged(5, "x", "1"), tagged(5, "x", "2")];
        let idx = build(&entries);
        let mut left = vec![tagged(5, "x", "1")].into_iter();
        let got = pick(|| Ok(left.next()), 5, idx.row(1).unwrap().hash, idx.occurrence(1)).unwrap();
        assert_eq!(got, None, "the second of two identical rows needs a second entry");
    }
}
