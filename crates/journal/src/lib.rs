//! The systemd journal and package history, read for the app's Logs screen and the daemon's events.

pub mod categories;
pub mod entry;
pub mod findings;
pub mod packages;
pub mod reader;

pub use entry::Entry;
