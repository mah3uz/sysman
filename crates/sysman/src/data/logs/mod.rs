//! The Logs screen's data: a compact index of journal entries and package transactions built by a cancellable
//! background scan, and the pure queries the screen runs over it.
//!
//! Flow: [`Logs::scan`] starts a [`Scan`] of a [`Scope`] (cancelling the previous one); read its index while it
//! fills with [`Scan::index`]; turn the search field's text into a [`Query`] with [`Query::parse`]; ask the
//! [`Index`] for [`matching`](Index::matching) rows, [`category_counts`](Index::category_counts),
//! [`histogram`](Index::histogram) and [`lanes`](Index::lanes). Free text and `pid:` need messages, which the
//! index does not hold: [`Logs::search`] streams them and yields ascending row ids to pass back as `hits`. A
//! [`Fetcher`] reads whole entries for visible rows and the inspector.

mod fetch;
mod index;
mod query;
mod scan;

use std::sync::Arc;

use anyhow::Result;
use journal::reader::{self, Access, Backend, Journal};

pub use fetch::{Around, Fetcher};
pub use index::{BootEnd, BootSummary, Bucket, CategoryCounts, Index, Lane, Matches, Row, Visible};
pub use query::{BootRef, Query, SearchSpec};
pub use scan::{Narrow, Scan, ScanState, Scope, Search, Span, now_us};

/// What the page needs for its access and backend banners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// `OwnOnly` means the system journal is unreadable: show the `systemd-journal` group banner.
    pub access: Access,
    /// `None` when neither libsystemd nor `journalctl` works.
    pub backend: Option<Backend>,
}

/// Checks the journal permission and which reader opens. Cheap enough for page open, not for every frame.
pub fn status() -> Status {
    Status {
        access: reader::access(),
        backend: Journal::open().ok().map(|j| j.backend()),
    }
}

/// Owns the current scan and search so a new one cancels the old.
#[derive(Default)]
pub struct Logs {
    scan: Option<Arc<Scan>>,
    search: Option<Arc<Search>>,
}

impl Logs {
    /// Starts scanning `scope`, cancelling the running scan and search first.
    pub fn scan(&mut self, scope: Scope) -> Arc<Scan> {
        self.cancel();
        let scan = Scan::start(scope);
        self.scan = Some(scan.clone());
        scan
    }

    pub fn current(&self) -> Option<&Arc<Scan>> {
        self.scan.as_ref()
    }

    /// Starts streaming `query`'s free text and pids over the current scan, cancelling the previous search.
    /// `None` (and the old search cancelled) when the query needs none or there is no scan.
    pub fn search(&mut self, query: &Query) -> Option<Arc<Search>> {
        if let Some(old) = self.search.take() {
            old.cancel();
        }
        let spec = query.search()?;
        let search = Search::start(self.scan.clone()?, spec);
        self.search = Some(search.clone());
        Some(search)
    }

    /// A reader for whole entries over the current scan's index.
    pub fn fetcher(&self) -> Result<Fetcher> {
        match &self.scan {
            Some(scan) => Fetcher::new(scan.clone()),
            None => anyhow::bail!("no scan has been started"),
        }
    }

    pub fn cancel(&mut self) {
        if let Some(s) = self.search.take() {
            s.cancel();
        }
        if let Some(s) = &self.scan {
            s.cancel();
        }
    }
}

impl Drop for Logs {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod real;
