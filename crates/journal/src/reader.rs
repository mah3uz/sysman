//! The systemd journal, read through `libsystemd.so.0` loaded at run time, or through `journalctl` when it does not
//! load.

mod cli;
mod sd;

use std::fs::File;
use std::io::ErrorKind;
use std::time::Duration;

use anyhow::{Result, bail};

use crate::Entry;

/// systemd's own default; set explicitly so both backends cut fields at the same length.
const DEFAULT_THRESHOLD: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Libsystemd,
    Journalctl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seek {
    Head,
    Tail,
    /// Microseconds since the Unix epoch.
    Realtime(u64),
    Cursor(String),
}

/// Which fields an entry is read with. `All` is every data field plus `__CURSOR`; `Only` reads just the listed
/// ones, `__CURSOR` included only when listed. `t_us` is always filled.
#[derive(Debug, Clone, Copy)]
pub enum Fields<'a> {
    All,
    Only(&'a [&'a str]),
}

/// An AND of OR-groups: an entry passes when every group has at least one `FIELD=value` it carries. Groups with
/// no matches are ignored; the empty filter passes everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter(pub Vec<Vec<(String, String)>>);

impl Filter {
    pub fn any<I, F, V>(mut self, matches: I) -> Self
    where
        I: IntoIterator<Item = (F, V)>,
        F: Into<String>,
        V: Into<String>,
    {
        self.0
            .push(matches.into_iter().map(|(f, v)| (f.into(), v.into())).collect());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boot {
    /// 32 lowercase hex digits, as in `_BOOT_ID`.
    pub id: String,
    pub first_us: u64,
    pub last_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The timeout passed.
    Nothing,
    /// Entries were appended; step forward to read them.
    Appended,
    /// Journal files were added, removed or rotated.
    FilesChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Full,
    /// Only the user's own journal is readable.
    OwnOnly,
}

/// A read position over the local machine's journal files: the system journal and every user journal the caller
/// may read, as `journalctl` shows by default. Use it from one thread at a time, and never in a forked child.
///
/// Stepping follows `sd_journal_next`/`sd_journal_previous`: after a seek the first step lands on the first entry
/// at or past the position in that direction, and each step after moves one entry from the last one returned.
/// Stepping forward from the tail finds nothing until entries are appended and then finds those, so following is
/// seek to the tail, then step forward and `wait` in turn. Set the filter before seeking.
///
/// The `journalctl` backend gives the same results with these limits:
/// - Every change of direction or of the field list, and every step at the end of the journal, starts a new
///   `journalctl` process, so it is only cheap for long runs in one direction.
/// - The data threshold cuts fields after `journalctl` has read them in full.
/// - An OR-group that matches several fields is expanded into OR-ed terms, at most 256 in all; a term that needs
///   two values of one field is dropped, so an entry carrying a field twice with both values does not match.
/// - `unique` splits values that contain a newline.
/// - `boots` needs a `journalctl` that prints `--list-boots` as JSON.
/// - `wait` starts `journalctl --follow` on its first call and only notices entries appended after that, and only
///   ones the filter passes; it never reports `FilesChanged`.
pub struct Journal(Inner);

enum Inner {
    Sd(sd::Sd),
    Cli(cli::Cli),
}

impl Journal {
    /// libsystemd when it loads with every function used, `journalctl` otherwise.
    pub fn open() -> Result<Journal> {
        match sd::api() {
            Ok(_) => Journal::open_with(Backend::Libsystemd),
            Err(_) => Journal::open_with(Backend::Journalctl),
        }
    }

    pub fn open_with(backend: Backend) -> Result<Journal> {
        Ok(Journal(match backend {
            Backend::Libsystemd => Inner::Sd(sd::Sd::open()?),
            Backend::Journalctl => Inner::Cli(cli::Cli::open()?),
        }))
    }

    pub fn backend(&self) -> Backend {
        match self.0 {
            Inner::Sd(_) => Backend::Libsystemd,
            Inner::Cli(_) => Backend::Journalctl,
        }
    }

    /// Replaces the previous filter.
    pub fn set_filter(&mut self, filter: &Filter) -> Result<()> {
        for (f, v) in filter.0.iter().flatten() {
            if !valid_field(f) {
                bail!("not a journal field name: {f:?}");
            }
            if v.contains('\0') {
                bail!("filter value for {f} contains NUL");
            }
        }
        match &mut self.0 {
            Inner::Sd(j) => j.set_filter(filter),
            Inner::Cli(j) => j.set_filter(filter),
        }
    }

    /// The most bytes of `FIELD=value` read per field; 0 is no limit. Values are cut to it, at a character
    /// boundary.
    pub fn set_data_threshold(&mut self, bytes: usize) -> Result<()> {
        match &mut self.0 {
            Inner::Sd(j) => j.set_threshold(bytes),
            Inner::Cli(j) => {
                j.threshold = bytes;
                Ok(())
            }
        }
    }

    pub fn seek(&mut self, to: Seek) -> Result<()> {
        match &mut self.0 {
            Inner::Sd(j) => j.seek(&to),
            Inner::Cli(j) => {
                j.seek(to);
                Ok(())
            }
        }
    }

    pub fn next(&mut self, fields: Fields) -> Result<Option<Entry>> {
        match &mut self.0 {
            Inner::Sd(j) => j.step(true, fields),
            Inner::Cli(j) => j.step(true, fields),
        }
    }

    pub fn previous(&mut self, fields: Fields) -> Result<Option<Entry>> {
        match &mut self.0 {
            Inner::Sd(j) => j.step(false, fields),
            Inner::Cli(j) => j.step(false, fields),
        }
    }

    /// Every value `field` takes anywhere in the journal, in no particular order; the filter does not apply.
    pub fn unique(&mut self, field: &str) -> Result<Vec<String>> {
        if !valid_field(field) {
            bail!("not a journal field name: {field:?}");
        }
        match &mut self.0 {
            Inner::Sd(j) => j.unique(field),
            Inner::Cli(j) => j.unique(field),
        }
    }

    /// Every boot in the journal, oldest first, the way `journalctl --list-boots` finds them; the filter does not
    /// apply. The read position is lost: seek before stepping again.
    pub fn boots(&mut self) -> Result<Vec<Boot>> {
        match &mut self.0 {
            Inner::Sd(j) => j.boots(),
            Inner::Cli(j) => j.boots(),
        }
    }

    /// Blocks until the journal changes or `timeout` passes. Changes before the first call are not reported, but
    /// stepping forward still reads the entries they appended.
    pub fn wait(&mut self, timeout: Duration) -> Result<Change> {
        match &mut self.0 {
            Inner::Sd(j) => j.wait(timeout),
            Inner::Cli(j) => j.wait(timeout),
        }
    }
}

/// `sd_journal_open` succeeds without read access to the system journal, so this tries the file itself. Anything
/// but a permission error, a missing journal included, counts as `Full`.
pub fn access() -> Access {
    let Ok(id) = std::fs::read_to_string("/etc/machine-id") else {
        return Access::Full;
    };
    for dir in ["/var/log/journal", "/run/log/journal"] {
        match File::open(format!("{dir}/{}/system.journal", id.trim())) {
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) if e.kind() == ErrorKind::PermissionDenied => return Access::OwnOnly,
            _ => return Access::Full,
        }
    }
    Access::Full
}

fn valid_field(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn wants(fields: Fields, name: &str) -> bool {
    match fields {
        Fields::All => name == "__CURSOR" || !name.starts_with("__"),
        Fields::Only(list) => list.contains(&name),
    }
}

/// Decodes a field's value as lossy UTF-8, cut so `FIELD=value` fits in `threshold` bytes.
fn value(name_len: usize, bytes: &[u8], threshold: usize) -> String {
    let max = if threshold == 0 {
        usize::MAX
    } else {
        threshold.saturating_sub(name_len + 1)
    };
    let cut = &bytes[..bytes.len().min(max)];
    match std::str::from_utf8(cut) {
        Ok(s) => s.to_owned(),
        Err(e) if e.error_len().is_none() && cut.len() < bytes.len() => {
            String::from_utf8_lossy(&cut[..e.valid_up_to()]).into_owned()
        }
        Err(_) => String::from_utf8_lossy(cut).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_cut_to_the_threshold_without_splitting_a_character() {
        assert_eq!(value(7, b"hello", 0), "hello", "0 means no limit");
        assert_eq!(
            value(7, b"hello world", 12),
            "hell",
            "MESSAGE= counts toward the threshold"
        );
        assert_eq!(
            value(1, "a\u{e9}".as_bytes(), 4),
            "a",
            "a cut through \u{e9} drops it rather than mangling it"
        );
        assert_eq!(value(1, b"a\xffb", 0), "a\u{fffd}b", "invalid bytes decode lossily");
    }

    #[test]
    fn field_names_follow_systemd_rules() {
        assert!(valid_field("_SYSTEMD_UNIT"));
        assert!(valid_field("MESSAGE_ID"));
        assert!(!valid_field("message"), "lower case");
        assert!(!valid_field("1FIELD"), "leading digit");
        assert!(!valid_field("A,B"), "would split journalctl's --output-fields");
        assert!(!valid_field(""));
    }

    #[test]
    fn invalid_filters_are_refused_before_reaching_a_backend() {
        let Ok(mut j) = Journal::open_with(Backend::Journalctl) else {
            return;
        };
        assert!(j.set_filter(&Filter::default().any([("bad name", "x")])).is_err());
        assert!(j.set_filter(&Filter::default().any([("UNIT", "a\0b")])).is_err());
    }

    #[test]
    fn a_journal_moves_between_threads() {
        fn send<T: Send>() {}
        send::<Journal>();
    }

    const SCAN: &[&str] = &[
        "PRIORITY",
        "_TRANSPORT",
        "SYSLOG_IDENTIFIER",
        "_SYSTEMD_UNIT",
        "_SYSTEMD_USER_UNIT",
        "_KERNEL_SUBSYSTEM",
        "MESSAGE_ID",
        "MESSAGE",
    ];

    fn this_boot() -> String {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
            .replace('-', "")
    }

    fn read_all(j: &mut Journal, fields: Fields) -> Vec<Entry> {
        let mut out = vec![];
        while let Some(e) = j.next(fields).unwrap() {
            out.push(e);
        }
        out
    }

    #[test]
    #[ignore = "reads this machine's journal"]
    fn smoke_reads_the_local_journal_alike_through_both_backends() {
        let mut sd = Journal::open().unwrap();
        assert_eq!(sd.backend(), Backend::Libsystemd);
        let mut cli = Journal::open_with(Backend::Journalctl).unwrap();
        println!("access: {:?}", access());

        let boots = sd.boots().unwrap();
        assert_eq!(boots.last().unwrap().id, this_boot());
        assert!(boots.windows(2).all(|w| w[0].first_us <= w[1].first_us), "oldest first");
        let cli_boots = cli.boots().unwrap();
        assert_eq!(boots.len(), cli_boots.len());
        let n = boots.len() - 1;
        let differ = boots[..n].iter().zip(&cli_boots).filter(|(a, b)| a != b).count();
        assert_eq!(
            differ, 0,
            "boots read differently; the current one is left out as its last entry moves"
        );

        let units = sd.unique("_TRANSPORT").unwrap();
        assert!(units.iter().any(|u| u == "kernel"), "{units:?}");
        let (mut a, mut b) = (units, cli.unique("_TRANSPORT").unwrap());
        a.sort();
        b.sort();
        assert_eq!(a, b);

        for j in [&mut sd, &mut cli] {
            j.seek(Seek::Tail).unwrap();
            assert_eq!(j.next(Fields::All).unwrap(), None, "nothing past the tail");
            let last = j.previous(Fields::All).unwrap().unwrap();
            assert!(last.get("__CURSOR").is_some() && last.get("_BOOT_ID").is_some());
            j.seek(Seek::Cursor(last.get("__CURSOR").unwrap().into())).unwrap();
            let again = j.next(Fields::Only(&["__CURSOR", "MESSAGE"])).unwrap().unwrap();
            assert_eq!(again.get("__CURSOR"), last.get("__CURSOR"));
            assert_eq!(again.fields.len(), 1 + usize::from(last.get("MESSAGE").is_some()));
        }

        let since = boots.last().unwrap().first_us;
        let filter = Filter::default().any([("_TRANSPORT", "kernel")]).any([
            ("PRIORITY", "0"),
            ("PRIORITY", "1"),
            ("PRIORITY", "2"),
            ("PRIORITY", "3"),
            ("PRIORITY", "4"),
        ]);
        let fields = Fields::Only(&["__CURSOR", "PRIORITY", "MESSAGE"]);
        let mut runs = vec![];
        for j in [&mut sd, &mut cli] {
            j.set_filter(&filter).unwrap();
            j.seek(Seek::Realtime(since)).unwrap();
            let fwd = read_all(j, fields);
            assert!(fwd.iter().all(|e| e.t_us >= since && e.priority() <= 4));
            j.seek(Seek::Tail).unwrap();
            let mut back = vec![];
            while let Some(e) = j.previous(fields).unwrap()
                && e.t_us >= since
            {
                back.push(e);
            }
            back.reverse();
            assert_eq!(fwd, back, "{:?}: forward and backward agree", j.backend());
            runs.push(fwd);
        }
        assert_eq!(runs[0], runs[1], "both backends read the same entries");
        println!("kernel warnings this boot: {}", runs[0].len());

        if runs[0].len() >= 3 {
            for j in [&mut sd, &mut cli] {
                j.seek(Seek::Realtime(since)).unwrap();
                let a = j.next(fields).unwrap().unwrap();
                let b = j.next(fields).unwrap().unwrap();
                assert_eq!(
                    j.previous(fields).unwrap().unwrap(),
                    a,
                    "{:?}: stepping back",
                    j.backend()
                );
                assert_eq!(j.next(fields).unwrap().unwrap(), b);
            }
        }
    }

    #[test]
    #[ignore = "writes one entry to this user's journal"]
    fn smoke_wait_reports_an_appended_entry() {
        for backend in [Backend::Libsystemd, Backend::Journalctl] {
            let mut j = Journal::open_with(backend).unwrap();
            let tag = format!("sysman-journal-test-{}-{backend:?}", std::process::id());
            j.set_filter(&Filter::default().any([("SYSLOG_IDENTIFIER", tag.as_str())]))
                .unwrap();
            let changed = j.wait(Duration::from_millis(200)).unwrap();
            if backend == Backend::Journalctl {
                assert_eq!(changed, Change::Nothing, "it only wakes for entries the filter passes");
            }
            // The first message is followed from the head, as nothing passed the filter; the second from the first.
            for message in ["follow test 1", "follow test 2"] {
                j.seek(Seek::Tail).unwrap();
                assert_eq!(j.next(Fields::All).unwrap(), None);
                let status = std::process::Command::new("systemd-cat")
                    .args(["-t", &tag, "echo", message])
                    .status()
                    .unwrap();
                assert!(status.success());
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let entry = loop {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "{backend:?}: no entry within 10 s"
                    );
                    j.wait(Duration::from_secs(1)).unwrap();
                    if let Some(e) = j.next(Fields::Only(&["MESSAGE"])).unwrap() {
                        break e;
                    }
                };
                assert_eq!(entry.message(), message, "{backend:?}");
            }
        }
    }

    #[test]
    #[ignore = "measures scans of this machine's journal; run with --release"]
    fn throughput_of_index_scans() {
        let week = 7 * 24 * 3600 * 1_000_000;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;
        for backend in [Backend::Libsystemd, Backend::Journalctl] {
            for (name, filter, seek) in [
                (
                    "this boot",
                    Filter::default().any([("_BOOT_ID", this_boot())]),
                    Seek::Head,
                ),
                ("last 7 days", Filter::default(), Seek::Realtime(now - week)),
            ] {
                let mut j = Journal::open_with(backend).unwrap();
                j.set_filter(&filter).unwrap();
                j.seek(seek).unwrap();
                let t = std::time::Instant::now();
                let mut n = 0u64;
                while j.next(Fields::Only(SCAN)).unwrap().is_some() {
                    n += 1;
                }
                let s = t.elapsed().as_secs_f64();
                println!(
                    "{backend:?} {name}: {n} entries in {s:.2} s, {:.0} entries/s",
                    n as f64 / s
                );
            }
        }
    }
}
