use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use super::{Boot, Change, DEFAULT_THRESHOLD, Fields, Filter, Seek, valid_field, value, wants};
use crate::Entry;

const MAX_TERMS: usize = 256;

pub(super) struct Cli {
    /// `None` when no entry can pass the filter.
    filter: Option<Vec<String>>,
    pos: Pos,
    stream: Option<Stream>,
    follow: Option<Follow>,
    /// Stepped forward from the tail and found nothing yet; see the libsystemd backend.
    past_tail: bool,
    pub(super) threshold: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pos {
    Seek(Seek),
    /// On this entry, as after a step.
    At(String),
}

struct Stream {
    child: Child,
    out: BufReader<ChildStdout>,
    forward: bool,
    fields: Option<Vec<String>>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Follow {
    child: Child,
    woke: mpsc::Receiver<()>,
}

impl Drop for Follow {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn journalctl(args: &[String]) -> Command {
    let mut c = Command::new("journalctl");
    c.args(["--no-pager", "--quiet"]).args(args).stdin(Stdio::null());
    c
}

fn with(options: &[&str], filter: &[String]) -> Vec<String> {
    options
        .iter()
        .map(|o| o.to_string())
        .chain(filter.iter().cloned())
        .collect()
}

fn run(args: &[String]) -> Result<Vec<u8>> {
    let out = journalctl(args).output().context("running journalctl")?;
    if !out.status.success() {
        bail!(
            "journalctl {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

impl Cli {
    pub(super) fn open() -> Result<Cli> {
        run(&["--version".into()])?;
        Ok(Cli {
            filter: Some(vec![]),
            pos: Pos::Seek(Seek::Head),
            stream: None,
            follow: None,
            past_tail: false,
            threshold: DEFAULT_THRESHOLD,
        })
    }

    pub(super) fn set_filter(&mut self, filter: &Filter) -> Result<()> {
        self.filter = filter_args(filter)?;
        self.stream = None;
        self.follow = None;
        Ok(())
    }

    pub(super) fn seek(&mut self, to: Seek) {
        self.pos = Pos::Seek(to);
        self.stream = None;
        self.past_tail = false;
    }

    pub(super) fn step(&mut self, forward: bool, fields: Fields) -> Result<Option<Entry>> {
        let Some(filter) = &self.filter else { return Ok(None) };
        if !forward && self.past_tail {
            self.pos = Pos::Seek(Seek::Tail);
            self.stream = None;
        }
        if self
            .stream
            .as_ref()
            .is_some_and(|s| s.forward != forward || !same_fields(&s.fields, fields))
        {
            self.stream = None;
        }
        let from_tail = self.stream.is_none() && forward && self.pos == Pos::Seek(Seek::Tail);
        if self.stream.is_none() {
            if from_tail {
                let last = run(&with(
                    &["--output=json", "--output-fields=__CURSOR", "--lines=1"],
                    filter,
                ))?;
                self.pos = match last.split(|&b| b == b'\n').find(|l| !l.is_empty()) {
                    Some(line) => Pos::At(parse_entry(line, Fields::Only(&[]), 0)?.1),
                    None => Pos::Seek(Seek::Head),
                };
            }
            let Some(pos) = position_args(&self.pos, forward) else {
                return Ok(None);
            };
            let mut args = vec!["--output=json".to_owned(), "--all".to_owned()];
            if let Fields::Only(list) = fields {
                if let Some(bad) = list.iter().find(|f| !valid_field(f)) {
                    bail!("not a journal field name: {bad:?}");
                }
                let data: Vec<&str> = list.iter().copied().filter(|f| !f.starts_with("__")).collect();
                args.push(format!(
                    "--output-fields={}",
                    if data.is_empty() {
                        "__CURSOR".into()
                    } else {
                        data.join(",")
                    }
                ));
            }
            args.extend(pos);
            args.extend(filter.iter().cloned());
            let mut child = journalctl(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .context("running journalctl")?;
            let out = BufReader::new(child.stdout.take().expect("stdout is piped"));
            let fields = match fields {
                Fields::All => None,
                Fields::Only(list) => Some(list.iter().map(|f| f.to_string()).collect()),
            };
            self.stream = Some(Stream {
                child,
                out,
                forward,
                fields,
            });
        }
        let stream = self.stream.as_mut().expect("started above");
        let mut line = vec![];
        if stream.out.read_until(b'\n', &mut line).context("reading journalctl")? == 0 {
            let mut stream = self.stream.take().expect("started above");
            let status = stream.child.wait().context("waiting for journalctl")?;
            if !status.success() {
                let mut err = String::new();
                if let Some(mut e) = stream.child.stderr.take() {
                    let _ = e.read_to_string(&mut err);
                }
                bail!("journalctl {status}: {}", err.trim());
            }
            self.past_tail |= from_tail;
            return Ok(None);
        }
        let (entry, cursor) = parse_entry(&line, fields, self.threshold)?;
        self.pos = Pos::At(cursor);
        self.past_tail = false;
        Ok(Some(entry))
    }

    pub(super) fn unique(&mut self, field: &str) -> Result<Vec<String>> {
        let out = run(&["-F".into(), field.into()])?;
        Ok(out
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|v| value(field.len(), v, self.threshold))
            .collect())
    }

    pub(super) fn boots(&mut self) -> Result<Vec<Boot>> {
        self.stream = None;
        parse_boots(&run(&["--list-boots".into(), "--output=json".into()])?)
    }

    pub(super) fn wait(&mut self, timeout: Duration) -> Result<Change> {
        let Some(filter) = &self.filter else {
            std::thread::sleep(timeout);
            return Ok(Change::Nothing);
        };
        if self.follow.is_none() {
            let args = with(
                &["--follow", "--lines=0", "--output=json", "--output-fields=__CURSOR"],
                filter,
            );
            let mut child = journalctl(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .context("running journalctl --follow")?;
            let mut out = BufReader::new(child.stdout.take().expect("stdout is piped"));
            let (tx, woke) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let mut line = vec![];
                while out.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
                    line.clear();
                    if let Err(mpsc::TrySendError::Disconnected(_)) = tx.try_send(()) {
                        break;
                    }
                }
            });
            self.follow = Some(Follow { child, woke });
        }
        match self.follow.as_ref().expect("started above").woke.recv_timeout(timeout) {
            Ok(()) => Ok(Change::Appended),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(Change::Nothing),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.follow = None;
                bail!("journalctl --follow exited")
            }
        }
    }
}

fn same_fields(have: &Option<Vec<String>>, fields: Fields) -> bool {
    match (have, fields) {
        (None, Fields::All) => true,
        (Some(have), Fields::Only(list)) => have.len() == list.len() && have.iter().zip(list).all(|(a, b)| a == b),
        _ => false,
    }
}

/// `None` when the step finds nothing, as `sd_journal_previous` right after seeking to the head does.
fn position_args(pos: &Pos, forward: bool) -> Option<Vec<String>> {
    let mut args = match pos {
        Pos::Seek(Seek::Head) if !forward => return None,
        Pos::Seek(Seek::Tail) if forward => return None,
        Pos::Seek(Seek::Head | Seek::Tail) => vec![],
        Pos::Seek(Seek::Realtime(t)) => {
            let at = format!("@{}.{:06}", t / 1_000_000, t % 1_000_000);
            vec![if forward {
                format!("--since={at}")
            } else {
                format!("--until={at}")
            }]
        }
        Pos::Seek(Seek::Cursor(c)) => vec![format!("--cursor={c}")],
        Pos::At(c) => vec![format!("--after-cursor={c}")],
    };
    if !forward {
        args.push("--reverse".into());
    }
    Some(args)
}

/// journalctl has no AND between OR-groups: it ORs terms split by `+`, ANDs fields within a term, and ORs values
/// of one field. So the filter is multiplied out, each term a set of allowed values per field. `None` when every
/// term needs two values of one field.
fn filter_args(filter: &Filter) -> Result<Option<Vec<String>>> {
    let mut terms: Vec<BTreeMap<&str, Vec<&str>>> = vec![BTreeMap::new()];
    for group in filter.0.iter().filter(|g| !g.is_empty()) {
        let mut alternatives: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (f, v) in group {
            alternatives.entry(f).or_default().push(v);
        }
        let mut next = vec![];
        for term in &terms {
            for (f, values) in &alternatives {
                let mut term = term.clone();
                match term.get_mut(f) {
                    Some(have) => {
                        have.retain(|v| values.contains(v));
                        if have.is_empty() {
                            continue;
                        }
                    }
                    None => {
                        term.insert(f, values.clone());
                    }
                }
                next.push(term);
            }
        }
        if next.len() > MAX_TERMS {
            bail!("filter needs more than {MAX_TERMS} journalctl terms");
        }
        terms = next;
    }
    if terms.is_empty() {
        return Ok(None);
    }
    let mut args = vec![];
    for (i, term) in terms.iter().enumerate() {
        if i > 0 {
            args.push("+".to_owned());
        }
        args.extend(
            term.iter()
                .flat_map(|(f, values)| values.iter().map(move |v| format!("{f}={v}"))),
        );
    }
    Ok(Some(args))
}

/// A field's value: a string, a byte array for data that is not printable UTF-8, or an array of either when the
/// entry has the field more than once (the first is kept, as the libsystemd backend does).
fn json_bytes(v: &Value) -> Option<Cow<'_, [u8]>> {
    match v {
        Value::String(s) => Some(Cow::Borrowed(s.as_bytes())),
        Value::Array(a) if a.iter().all(Value::is_u64) => Some(Cow::Owned(
            a.iter()
                .filter_map(|b| b.as_u64().and_then(|b| u8::try_from(b).ok()))
                .collect(),
        )),
        Value::Array(a) => a.first().and_then(json_bytes),
        _ => None,
    }
}

/// One line of `journalctl -o json`: the entry and its cursor.
fn parse_entry(line: &[u8], fields: Fields, threshold: usize) -> Result<(Entry, String)> {
    let obj: Map<String, Value> = serde_json::from_slice(line).context("journalctl JSON")?;
    let mut e = Entry::default();
    let (mut t_us, mut cursor) = (None, None);
    for (k, v) in obj {
        match k.as_str() {
            "__REALTIME_TIMESTAMP" => t_us = v.as_str().and_then(|s| s.parse().ok()),
            "__CURSOR" => cursor = v.as_str().map(str::to_owned),
            _ if k.starts_with("__") || !wants(fields, &k) => {}
            _ => {
                if let Some(bytes) = json_bytes(&v) {
                    let v = value(k.len(), &bytes, threshold);
                    e.fields.insert(k, v);
                }
            }
        }
    }
    e.t_us = t_us.context("journalctl entry without __REALTIME_TIMESTAMP")?;
    let cursor = cursor.context("journalctl entry without __CURSOR")?;
    if wants(fields, "__CURSOR") {
        e.fields.insert("__CURSOR".into(), cursor.clone());
    }
    Ok((e, cursor))
}

fn parse_boots(json: &[u8]) -> Result<Vec<Boot>> {
    if json.iter().all(u8::is_ascii_whitespace) {
        return Ok(vec![]);
    }
    let list: Vec<Map<String, Value>> = serde_json::from_slice(json).context("journalctl --list-boots JSON")?;
    let mut boots = list
        .iter()
        .map(|b| {
            let index = b.get("index").and_then(Value::as_i64);
            let id = b.get("boot_id").and_then(Value::as_str);
            let first = b.get("first_entry").and_then(Value::as_u64);
            let last = b.get("last_entry").and_then(Value::as_u64);
            match (index, id, first, last) {
                (Some(i), Some(id), Some(first_us), Some(last_us)) => Ok((
                    i,
                    Boot {
                        id: id.to_owned(),
                        first_us,
                        last_us,
                    },
                )),
                _ => bail!("journalctl --list-boots entry without index, boot_id, first_entry or last_entry"),
            }
        })
        .collect::<Result<Vec<_>>>()?;
    boots.sort_by_key(|(i, _)| *i);
    Ok(boots.into_iter().map(|(_, b)| b).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"__CURSOR":"s=1;i=2","__REALTIME_TIMESTAMP":"1700000000000001","__MONOTONIC_TIMESTAMP":"5","_BOOT_ID":"0123456789abcdef0123456789abcdef","PRIORITY":"3","MESSAGE":[27,91,51,49,109,104,105,255],"_TRANSPORT":["stdout","journal"],"BIG":null,"__SEQNUM":"9"}"#;

    #[test]
    fn json_entries_decode_byte_arrays_and_repeated_fields() {
        let (e, cursor) = parse_entry(LINE.as_bytes(), Fields::All, 0).unwrap();
        assert_eq!(cursor, "s=1;i=2");
        assert_eq!(e.t_us, 1_700_000_000_000_001);
        assert_eq!(e.message(), "\x1b[31mhi\u{fffd}", "a byte array is lossy UTF-8");
        assert_eq!(
            e.get("_TRANSPORT"),
            Some("stdout"),
            "a repeated field keeps its first value"
        );
        assert_eq!(e.get("BIG"), None, "journalctl prints null for data it would not show");
        assert_eq!(e.get("__CURSOR"), Some("s=1;i=2"));
        assert_eq!(
            e.get("__MONOTONIC_TIMESTAMP"),
            None,
            "the libsystemd backend has no such field"
        );
        assert_eq!(e.get("__SEQNUM"), None);
    }

    #[test]
    fn json_entries_keep_only_the_fields_asked_for() {
        let (e, cursor) = parse_entry(LINE.as_bytes(), Fields::Only(&["PRIORITY", "MESSAGE"]), 12).unwrap();
        assert_eq!(cursor, "s=1;i=2", "kept for the position even when not asked for");
        let names: Vec<_> = e.fields.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            ["MESSAGE", "PRIORITY"],
            "_BOOT_ID is always printed but was not asked for"
        );
        assert_eq!(e.message(), "\x1b[31", "MESSAGE= and 4 bytes fit in 12");
    }

    #[test]
    fn json_entries_without_a_timestamp_or_cursor_are_errors() {
        assert!(parse_entry(br#"{"__CURSOR":"c","MESSAGE":"x"}"#, Fields::All, 0).is_err());
        assert!(parse_entry(br#"{"__REALTIME_TIMESTAMP":"1","MESSAGE":"x"}"#, Fields::All, 0).is_err());
        assert!(parse_entry(b"not json", Fields::All, 0).is_err());
    }

    #[test]
    fn boots_come_oldest_first() {
        let json = br#"[{"index":0,"boot_id":"bb","first_entry":30,"last_entry":40},
            {"index":-1,"boot_id":"aa","first_entry":10,"last_entry":20}]"#;
        let boots = parse_boots(json).unwrap();
        assert_eq!(
            boots[0],
            Boot {
                id: "aa".into(),
                first_us: 10,
                last_us: 20
            }
        );
        assert_eq!(boots[1].id, "bb");
        assert_eq!(parse_boots(b"\n").unwrap(), vec![], "an empty journal");
        assert!(parse_boots(br#"[{"index":0,"boot_id":"aa"}]"#).is_err());
    }

    #[test]
    fn positions_map_to_journalctl_options() {
        let at = |p: Pos, fwd| position_args(&p, fwd);
        assert_eq!(at(Pos::Seek(Seek::Head), true), Some(vec![]));
        assert_eq!(at(Pos::Seek(Seek::Head), false), None, "nothing before the head");
        assert_eq!(at(Pos::Seek(Seek::Tail), false), Some(vec!["--reverse".into()]));
        assert_eq!(
            at(Pos::Seek(Seek::Realtime(1_700_000_000_000_042)), true),
            Some(vec!["--since=@1700000000.000042".into()])
        );
        assert_eq!(
            at(Pos::Seek(Seek::Realtime(5)), false),
            Some(vec!["--until=@0.000005".into(), "--reverse".into()])
        );
        assert_eq!(
            at(Pos::Seek(Seek::Cursor("c".into())), false),
            Some(vec!["--cursor=c".into(), "--reverse".into()])
        );
        assert_eq!(
            at(Pos::At("c".into()), true),
            Some(vec!["--after-cursor=c".into()]),
            "a step leaves the entry"
        );
    }

    /// The filter's meaning: every non-empty group has a match the entry carries.
    fn passes(filter: &Filter, entry: &[(&str, &str)]) -> bool {
        filter
            .0
            .iter()
            .filter(|g| !g.is_empty())
            .all(|g| g.iter().any(|(f, v)| entry.contains(&(f.as_str(), v.as_str()))))
    }

    /// journalctl's meaning of its match arguments.
    fn journalctl_passes(args: &[String], entry: &[(&str, &str)]) -> bool {
        args.split(|a| a == "+").any(|term| {
            let mut by_field: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
            for m in term {
                let (f, v) = m.split_once('=').unwrap();
                by_field.entry(f).or_default().push(v);
            }
            by_field
                .iter()
                .all(|(f, vs)| vs.iter().any(|v| entry.contains(&(*f, *v))))
        })
    }

    #[test]
    fn filters_translate_to_journalctl_matches_with_the_same_meaning() {
        let filters = [
            Filter::default(),
            Filter::default().any([("_TRANSPORT", "kernel")]),
            Filter::default()
                .any([("PRIORITY", "3"), ("PRIORITY", "4")])
                .any([("_TRANSPORT", "kernel")]),
            Filter::default()
                .any([("_SYSTEMD_UNIT", "a.service"), ("UNIT", "a.service")])
                .any([("PRIORITY", "3")]),
            Filter::default()
                .any([("_SYSTEMD_UNIT", "a.service"), ("_SYSTEMD_USER_UNIT", "b.service")])
                .any([("PRIORITY", "3"), ("_TRANSPORT", "kernel")]),
            Filter::default()
                .any([("PRIORITY", "3"), ("PRIORITY", "4")])
                .any([("PRIORITY", "4"), ("PRIORITY", "5")]),
            Filter::default()
                .any(Vec::<(String, String)>::new())
                .any([("PRIORITY", "3")]),
        ];
        let fields = [
            ("PRIORITY", ["3", "4", "5"]),
            ("_TRANSPORT", ["kernel", "journal", "stdout"]),
        ];
        let units = ["a.service", "b.service", "c.service"];
        let mut entries = vec![];
        for p in fields[0].1 {
            for t in fields[1].1 {
                for u in units {
                    for unit_field in ["_SYSTEMD_UNIT", "_SYSTEMD_USER_UNIT", "UNIT"] {
                        entries.push(vec![("PRIORITY", p), ("_TRANSPORT", t), (unit_field, u)]);
                    }
                }
            }
        }
        for filter in &filters {
            let args = filter_args(filter).unwrap().expect("satisfiable");
            for e in &entries {
                assert_eq!(
                    passes(filter, e),
                    journalctl_passes(&args, e),
                    "{filter:?} on {e:?} as {args:?}"
                );
            }
        }
        assert_eq!(
            filter_args(&Filter::default().any([("PRIORITY", "3")]).any([("PRIORITY", "4")])).unwrap(),
            None,
            "a single-valued field cannot be both"
        );
    }

    #[test]
    fn filters_too_large_to_multiply_out_are_refused() {
        let mut f = Filter::default();
        for g in 0..9 {
            f = f.any([(format!("A{g}"), "1"), (format!("B{g}"), "1")]);
        }
        assert!(filter_args(&f).is_err(), "2^9 terms");
    }
}
