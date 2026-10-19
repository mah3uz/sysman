//! The Logs screen's data: a compact index of journal entries and package transactions built by a cancellable
//! background scan, and the pure queries the screen runs over it.
//!
//! Flow: [`Logs::scan`] starts a [`Scan`] of a [`Scope`] (cancelling the previous one); read its index while it
//! fills with [`Scan::index`]; turn the search field's text into a [`Query`] with [`Query::parse`]; ask the
//! [`Index`] for [`matching`](Index::matching) rows, [`category_counts`](Index::category_counts),
//! [`histogram`](Index::histogram) and [`lanes`](Index::lanes). Free text and `pid:` need messages, which the
//! index does not hold: [`Logs::search`] streams them and yields ascending row ids to pass back as `hits`. A
//! [`Fetcher`] reads whole entries for visible rows and the inspector.

#[cfg(debug_assertions)]
mod demo;
mod fetch;
mod index;
mod query;
mod scan;

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use journal::reader::{self, Access, Backend, Journal};

pub use fetch::{Around, Fetcher};
pub use index::{BootEnd, BootSummary, Bucket, CategoryCounts, Index, Lane, Matches, Row, Visible, end_note};
pub use query::{BootRef, Query, SearchSpec};
pub use scan::{Narrow, ROW_CAP, Scan, ScanState, Scope, Search, Span, now_us};

/// What the page needs for its access and backend banners, and to tell "nothing happened" from "can't tell".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// `OwnOnly` means the system journal is unreadable: show the `systemd-journal` group banner.
    pub access: Access,
    /// `None` when neither libsystemd nor `journalctl` works.
    pub backend: Option<Backend>,
    /// No journal directory exists, as under WSL without systemd or in a container.
    pub no_journal: bool,
    /// systemd-coredump records crashes in the journal; without it a crash leaves no entry to find.
    pub crash_reports: bool,
    /// A package manager whose history is not read, when no history that is read was found.
    pub unread_packages: Option<&'static str>,
}

/// Checks the journal permission and which reader opens. Cheap enough for page open, not for every frame.
pub fn status() -> Status {
    #[allow(unused_mut)]
    let mut s = Status {
        access: access(),
        backend: open_journal().ok().map(|j| j.backend()),
        no_journal: !["/var/log/journal", "/run/log/journal"]
            .iter()
            .any(|d| Path::new(d).is_dir()),
        crash_reports: crash_reports(),
        unread_packages: unread_packages(),
    };
    #[cfg(debug_assertions)]
    {
        let var = |name: &str| std::env::var(format!("SYSMAN_DEV_LOGS_{name}")).ok();
        s.no_journal |= var("JOURNAL").as_deref() == Some("none");
        s.crash_reports &= var("COREDUMP").as_deref() != Some("missing");
        if let Some(p) = var("PACKAGES") {
            s.unread_packages = Some(if p == "zypper" { "zypper" } else { "dnf" });
        }
    }
    s
}

/// The journal, or with `SYSMAN_DEV_LOGS_DEMO` the made-up week of [`demo`].
pub fn open_journal() -> Result<Journal> {
    #[cfg(debug_assertions)]
    if let Some(demo) = demo::get() {
        return Journal::open_files(std::slice::from_ref(&demo.journal));
    }
    Journal::open()
}

/// The journal permission; `SYSMAN_DEV_LOGS_ACCESS=own` pretends only the user's own journal is readable.
pub fn access() -> Access {
    #[cfg(debug_assertions)]
    if std::env::var("SYSMAN_DEV_LOGS_ACCESS").as_deref() == Ok("own") {
        return Access::OwnOnly;
    }
    reader::access()
}

fn crash_reports() -> bool {
    let on_path =
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("coredumpctl").is_file()));
    let piped = std::fs::read_to_string("/proc/sys/kernel/core_pattern")
        .map_or(true, |p| p.starts_with('|') && p.contains("systemd-coredump"));
    on_path && piped
}

fn unread_packages() -> Option<&'static str> {
    let read = ["/var/log/pacman.log", "/var/log/dpkg.log", "/var/log/apt/history.log"];
    if read.iter().any(|p| Path::new(p).exists()) {
        return None;
    }
    [("dnf", "/usr/bin/dnf"), ("zypper", "/usr/bin/zypper")]
        .into_iter()
        .find(|(_, bin)| Path::new(bin).exists())
        .map(|(name, _)| name)
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
