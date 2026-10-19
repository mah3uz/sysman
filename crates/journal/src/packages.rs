//! Package-manager history from pacman, dpkg and apt logs, grouped into transactions.

use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::Result;
use jiff::civil::DateTime;
use jiff::tz::{Offset, TimeZone};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Pacman,
    Dpkg,
    Apt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Installed,
    Upgraded,
    Downgraded,
    Removed,
    Reinstalled,
    Purged,
}

/// `old` is set for upgrades, downgrades, removals and reinstalls; `new` for installs, upgrades,
/// downgrades and reinstalls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub action: Action,
    pub name: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    pub manager: Manager,
    pub start_us: u64,
    pub end_us: u64,
    pub command: Option<String>,
    pub requested_by: Option<String>,
    pub changes: Vec<Change>,
    pub warnings: Vec<String>,
}

impl Transaction {
    fn new(manager: Manager, start_us: u64) -> Self {
        Self {
            manager,
            start_us,
            end_us: start_us,
            command: None,
            requested_by: None,
            changes: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

fn to_us(ts: jiff::Timestamp) -> u64 {
    u64::try_from(ts.as_microsecond()).unwrap_or(0)
}

fn local_us(s: &str, tz: &TimeZone) -> Option<u64> {
    let dt: DateTime = s.parse().ok()?;
    Some(to_us(tz.to_timestamp(dt).ok()?))
}

fn alpm_us(s: &str) -> Option<u64> {
    let (dt, off) = (s.get(..19)?, s.get(19..)?);
    let dt: DateTime = dt.parse().ok()?;
    let sign = match off.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = off[1..].chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let secs = sign * (digits[..2].parse::<i32>().ok()? * 3600 + digits[2..].parse::<i32>().ok()? * 60);
    let offset = Offset::from_seconds(secs).ok()?;
    Some(to_us(offset.to_timestamp(dt).ok()?))
}

pub fn parse_pacman(text: &str) -> Vec<Transaction> {
    let mut out = Vec::new();
    let mut command: Option<String> = None;
    let mut current: Option<Transaction> = None;
    let mut last_us = 0;

    for line in text.lines() {
        let Some((ts, tag, msg)) = alpm_line(line) else { continue };
        let Some(t) = alpm_us(ts) else { continue };
        last_us = t;
        match (tag, msg) {
            ("PACMAN", m) => {
                if let Some(c) = m.strip_prefix("Running '").and_then(|c| c.strip_suffix('\'')) {
                    command = Some(c.to_string());
                }
            }
            ("ALPM", "transaction started") => {
                let mut tx = Transaction::new(Manager::Pacman, t);
                tx.command = command.take();
                if let Some(unfinished) = current.replace(tx) {
                    out.push(unfinished);
                }
            }
            ("ALPM", "transaction completed" | "transaction failed") => {
                if let Some(mut tx) = current.take() {
                    tx.end_us = t;
                    out.push(tx);
                }
            }
            ("ALPM", m) => {
                let Some(tx) = current.as_mut() else { continue };
                tx.end_us = t;
                if let Some(w) = m.strip_prefix("warning: ").or_else(|| m.strip_prefix("error: ")) {
                    tx.warnings.push(w.to_string());
                } else if let Some(c) = alpm_change(m) {
                    tx.changes.push(c);
                }
            }
            _ => {}
        }
    }
    if let Some(mut tx) = current {
        tx.end_us = last_us.max(tx.start_us);
        out.push(tx);
    }
    out.retain(|t| !t.changes.is_empty());
    out
}

fn alpm_line(line: &str) -> Option<(&str, &str, &str)> {
    let rest = line.strip_prefix('[')?;
    let (ts, rest) = rest.split_once("] [")?;
    let (tag, msg) = rest.split_once("] ")?;
    Some((ts, tag, msg.trim_end()))
}

fn alpm_change(msg: &str) -> Option<Change> {
    let (verb, rest) = msg.split_once(' ')?;
    let action = match verb {
        "installed" => Action::Installed,
        "upgraded" => Action::Upgraded,
        "downgraded" => Action::Downgraded,
        "removed" => Action::Removed,
        "reinstalled" => Action::Reinstalled,
        _ => return None,
    };
    let (name, versions) = rest.split_once(" (")?;
    let versions = versions.strip_suffix(')')?;
    let (old, new) = match action {
        Action::Upgraded | Action::Downgraded => {
            let (o, n) = versions.split_once(" -> ")?;
            (Some(o.to_string()), Some(n.to_string()))
        }
        Action::Installed => (None, Some(versions.to_string())),
        Action::Removed | Action::Purged => (Some(versions.to_string()), None),
        Action::Reinstalled => (Some(versions.to_string()), Some(versions.to_string())),
    };
    Some(Change { action, name: name.to_string(), old, new })
}

pub fn parse_dpkg(text: &str, tz: &TimeZone) -> Vec<Transaction> {
    let mut out = Vec::new();
    let mut current: Option<Transaction> = None;
    let mut configured = false;

    for line in text.lines() {
        let line = line.trim_end();
        let (Some(date), Some(rest)) = (line.get(..19), line.get(20..)) else { continue };
        let Some(t) = local_us(date, tz) else { continue };
        let mut words = rest.split(' ');
        let Some(verb) = words.next() else { continue };

        if verb == "startup" {
            let kind = (words.next(), words.next());
            let joins = kind == (Some("packages"), Some("configure"))
                && current.as_ref().is_some_and(|c| !configured && !c.changes.is_empty());
            if joins {
                configured = true;
            } else {
                out.extend(current.take());
                current = Some(Transaction::new(Manager::Dpkg, t));
                configured = kind == (Some("packages"), Some("configure"));
            }
        }

        let action = match verb {
            "install" => Action::Installed,
            "upgrade" => Action::Upgraded,
            "remove" => Action::Removed,
            "purge" => Action::Purged,
            _ => Action::Reinstalled,
        };
        let is_change = matches!(verb, "install" | "upgrade" | "remove" | "purge");
        let tx = current.get_or_insert_with(|| Transaction::new(Manager::Dpkg, t));
        tx.end_us = tx.end_us.max(t);
        if !is_change {
            continue;
        }
        let (Some(name), Some(old), Some(new)) = (words.next(), words.next(), words.next()) else {
            continue;
        };
        let version = |v: &str| (v != "<none>").then(|| v.to_string());
        let (old, new) = (version(old), version(new));
        if action == Action::Purged
            && let Some(prev) = tx.changes.iter_mut().find(|c| c.name == name && c.action == Action::Removed)
        {
            prev.action = Action::Purged;
            continue;
        }
        tx.changes.push(Change { action, name: name.to_string(), old, new });
    }
    out.extend(current);
    out.retain(|t| !t.changes.is_empty());
    out
}

pub fn parse_apt_history(text: &str, tz: &TimeZone) -> Vec<Transaction> {
    let mut out = Vec::new();
    let mut current: Option<Transaction> = None;

    for line in text.lines() {
        let line = line.trim_end();
        let Some((key, value)) = line.split_once(": ") else {
            if line.is_empty() {
                out.extend(current.take());
            }
            continue;
        };
        if key == "Start-Date" {
            out.extend(current.take());
            let when = value.split_whitespace().collect::<Vec<_>>().join(" ");
            if let Some(t) = local_us(&when, tz) {
                current = Some(Transaction::new(Manager::Apt, t));
            }
            continue;
        }
        let Some(tx) = current.as_mut() else { continue };
        match key {
            "Commandline" => tx.command = Some(value.to_string()),
            "Requested-By" => tx.requested_by = Some(value.to_string()),
            "Error" => tx.warnings.push(value.to_string()),
            "End-Date" => {
                let when = value.split_whitespace().collect::<Vec<_>>().join(" ");
                if let Some(t) = local_us(&when, tz) {
                    tx.end_us = t.max(tx.start_us);
                }
                out.extend(current.take());
            }
            _ => {
                let action = match key {
                    "Install" => Action::Installed,
                    "Upgrade" => Action::Upgraded,
                    "Downgrade" => Action::Downgraded,
                    "Remove" => Action::Removed,
                    "Purge" => Action::Purged,
                    "Reinstall" => Action::Reinstalled,
                    _ => continue,
                };
                tx.changes.extend(value.split("), ").filter_map(|e| apt_change(action, e)));
            }
        }
    }
    out.extend(current);
    out.retain(|t| !t.changes.is_empty());
    out
}

fn apt_change(action: Action, entry: &str) -> Option<Change> {
    let (name, versions) = entry.trim().split_once(" (")?;
    let versions = versions.strip_suffix(')').unwrap_or(versions);
    let mut parts = versions.split(", ");
    let first = parts.next()?.to_string();
    let (old, new) = match action {
        Action::Upgraded | Action::Downgraded => (Some(first), Some(parts.next()?.to_string())),
        Action::Installed => (None, Some(first)),
        Action::Removed | Action::Purged => (Some(first), None),
        Action::Reinstalled => (Some(first.clone()), Some(first)),
    };
    Some(Change { action, name: name.to_string(), old, new })
}

/// Reads whichever of pacman, dpkg and apt history exist on this machine; transactions that
/// ended before `since_us` are dropped.
pub fn read_history(since_us: u64) -> Result<Vec<Transaction>> {
    Ok(read_history_in(Path::new("/var/log"), since_us, &TimeZone::system()))
}

fn read_history_in(log_dir: &Path, since_us: u64, tz: &TimeZone) -> Vec<Transaction> {
    let mut all = Vec::new();
    if let Some(text) = read_text(&log_dir.join("pacman.log")) {
        all.extend(parse_pacman(&text));
    }
    let mut dpkg = Vec::new();
    for text in rotated_texts(log_dir, "dpkg.log") {
        dpkg.extend(parse_dpkg(&text, tz));
    }
    let mut apt = Vec::new();
    for text in rotated_texts(&log_dir.join("apt"), "history.log") {
        apt.extend(parse_apt_history(&text, tz));
    }
    dpkg.retain(|d| !apt.iter().any(|a| d.start_us <= a.end_us && d.end_us >= a.start_us));
    all.extend(dpkg);
    all.extend(apt);
    all.retain(|t| t.end_us >= since_us);
    all.sort_by_key(|t| t.start_us);
    all
}

fn rotated_texts(dir: &Path, base: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n == base || n.strip_prefix(base).is_some_and(|s| s.starts_with('.'))
        })
        .map(|e| e.path())
        .collect();
    paths.sort();
    paths.iter().filter_map(|p| read_text(p)).collect()
}

fn read_text(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let bytes = if path.extension().is_some_and(|e| e == "gz") {
        let mut plain = Vec::new();
        flate2::read::MultiGzDecoder::new(bytes.as_slice()).read_to_end(&mut plain).ok()?;
        plain
    } else {
        bytes
    };
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tz(hours: i8) -> TimeZone {
        Offset::constant(hours).to_time_zone()
    }

    fn us(s: &str) -> u64 {
        to_us(s.parse::<jiff::Timestamp>().unwrap())
    }

    const PACMAN: &str = "\
[2026-03-01T10:00:00+0600] [PACMAN] Running 'pacman -S foo'\r
[2026-03-01T10:00:01+0600] [PACMAN] synchronizing package lists
[2026-03-01T10:00:02+0600] [ALPM] transaction started
[2026-03-01T10:00:02+0600] [ALPM] installed foo (1.0-1)
[2026-03-01T10:00:03+0600] [ALPM-SCRIPTLET] noise
[2026-03-01T10:00:03+0600] [ALPM] transaction completed
this line is garbage
[2026-03-02T11:00:00+0000] [PACMAN] Running 'pacman -Syu'
[2026-03-02T11:00:05+0000] [ALPM] transaction started
[2026-03-02T11:00:06+0000] [ALPM] warning: /etc/foo installed as /etc/foo.pacnew
[2026-03-02T11:00:07+0000] [ALPM] upgraded bar (2.0-1 -> 2.1-1)
[2026-03-02T11:00:08+0000] [ALPM] removed baz (3.0-1)
[2026-03-02T11:00:09+0000] [ALPM] downgraded qux (5-1 -> 4-1)
[2026-03-02T11:00:09+0000] [ALPM] reinstalled quux (1-1)
[2026-03-02T11:00:10+0000] [ALPM] transaction completed
[2026-03-03T09:00:00+0000] [PACMAN] Running 'pacman -Rns unfinished'
[2026-03-03T09:00:01+0000] [ALPM] transaction started
[2026-03-03T09:00:02+0000] [ALPM] removed gone (1-1)
[2026-03-03T09:00:03+0000] [ALPM] remo";

    #[test]
    fn a_transactions_command_is_the_pacman_invocation_that_started_it() {
        let txs = parse_pacman(PACMAN);
        assert_eq!(txs[0].command.as_deref(), Some("pacman -S foo"));
        assert_eq!(txs[1].command.as_deref(), Some("pacman -Syu"));
        assert_eq!(txs[0].manager, Manager::Pacman);
    }

    #[test]
    fn an_upgrade_keeps_both_versions_so_the_user_can_see_what_changed() {
        let txs = parse_pacman(PACMAN);
        let up = &txs[1].changes[0];
        assert_eq!(up.action, Action::Upgraded);
        assert_eq!((up.old.as_deref(), up.new.as_deref()), (Some("2.0-1"), Some("2.1-1")));
        let down = &txs[1].changes[2];
        assert_eq!((down.action, down.old.as_deref(), down.new.as_deref()), (Action::Downgraded, Some("5-1"), Some("4-1")));
    }

    #[test]
    fn pacman_timestamps_keep_their_own_offset() {
        let txs = parse_pacman(PACMAN);
        assert_eq!(txs[0].start_us, us("2026-03-01T04:00:02Z"));
        assert_eq!(txs[1].start_us, us("2026-03-02T11:00:05Z"));
        assert_eq!(txs[1].end_us, us("2026-03-02T11:00:10Z"));
    }

    #[test]
    fn pacman_warnings_belong_to_their_transaction_not_the_changes() {
        let txs = parse_pacman(PACMAN);
        assert_eq!(txs[1].warnings, vec!["/etc/foo installed as /etc/foo.pacnew"]);
        assert_eq!(txs[1].changes.len(), 4);
    }

    #[test]
    fn a_transaction_cut_off_by_a_truncated_log_is_kept_with_what_was_read() {
        let txs = parse_pacman(PACMAN);
        assert_eq!(txs.len(), 3);
        assert_eq!(txs[2].changes.len(), 1);
        assert_eq!(txs[2].end_us, us("2026-03-03T09:00:03Z"));
    }

    const DPKG: &str = "\
2026-03-01 10:00:00 startup archives unpack
2026-03-01 10:00:01 install foo:amd64 <none> 1.0
2026-03-01 10:00:02 status half-installed foo:amd64 1.0
2026-03-01 10:00:03 upgrade bar:amd64 2.0 2.1
2026-03-01 10:00:04 status unpacked bar:amd64 2.1
2026-03-01 10:00:05 startup packages configure
2026-03-01 10:00:06 configure foo:amd64 1.0 <none>
2026-03-01 10:00:07 trigproc man-db:amd64 2.0 <none>
2026-03-01 10:00:08 status installed foo:amd64 1.0
bad line
2026-03-02 12:00:00 startup packages remove
2026-03-02 12:00:01 remove old:amd64 1.0 <none>
2026-03-02 12:00:03 purge old:amd64 1.0 <none>
2026-03-02 12:00:04 startup packages purge
2026-03-02 12:00:05 purge never-removed:amd64 1.0 <none>
2026-03-02 12:00:06 install trunc";

    #[test]
    fn dpkg_noise_does_not_hide_the_upgrades_in_a_run() {
        let txs = parse_dpkg(DPKG, &tz(0));
        assert_eq!(txs[0].manager, Manager::Dpkg);
        let names: Vec<_> = txs[0].changes.iter().map(|c| (c.action, c.name.as_str())).collect();
        assert_eq!(names, vec![(Action::Installed, "foo:amd64"), (Action::Upgraded, "bar:amd64")]);
        let up = &txs[0].changes[1];
        assert_eq!((up.old.as_deref(), up.new.as_deref()), (Some("2.0"), Some("2.1")));
    }

    #[test]
    fn the_configure_step_belongs_to_the_run_that_unpacked() {
        let txs = parse_dpkg(DPKG, &tz(0));
        assert_eq!(txs[0].start_us, us("2026-03-01T10:00:00Z"));
        assert_eq!(txs[0].end_us, us("2026-03-01T10:00:08Z"));
    }

    #[test]
    fn a_removal_followed_by_its_purge_is_one_purged_change() {
        let txs = parse_dpkg(DPKG, &tz(0));
        let purged: Vec<_> = txs.iter().flat_map(|t| &t.changes).filter(|c| c.name == "old:amd64").collect();
        assert_eq!(purged.len(), 1);
        assert_eq!(purged[0].action, Action::Purged);
    }

    #[test]
    fn dpkg_local_times_are_converted_with_the_given_zone() {
        let txs = parse_dpkg(DPKG, &tz(10));
        assert_eq!(txs[0].start_us, us("2026-03-01T00:00:00Z"));
    }

    const APT: &str = "\
Start-Date: 2026-03-01  10:00:00
Commandline: apt install foo
Requested-By: user (1000)
Install: foo:amd64 (1.0, automatic), libfoo:amd64 (1.0)
Upgrade: bar:amd64 (2.0, 2.1), baz:amd64 (1, 2)
End-Date: 2026-03-01  10:00:09

Start-Date: 2026-03-02  09:00:00
Commandline: apt remove old
Remove: old:amd64 (1.0)
Purge: older:amd64 (0.9)
Downgrade: down:amd64 (3, 2)
Reinstall: again:amd64 (7)
Error: Sub-process /usr/bin/dpkg returned an error code (1)
End-Date: 2026-03-02  09:00:30

Start-Date: 2026-03-03  09:00:00
Commandline: apt install cut
Install: cut:amd64 (1.0), tail:amd6";

    #[test]
    fn apt_blocks_carry_the_command_and_who_asked() {
        let txs = parse_apt_history(APT, &tz(0));
        assert_eq!(txs[0].manager, Manager::Apt);
        assert_eq!(txs[0].command.as_deref(), Some("apt install foo"));
        assert_eq!(txs[0].requested_by.as_deref(), Some("user (1000)"));
        assert_eq!(txs[0].start_us, us("2026-03-01T10:00:00Z"));
        assert_eq!(txs[0].end_us, us("2026-03-01T10:00:09Z"));
    }

    #[test]
    fn apt_upgrade_lines_keep_old_and_new_and_automatic_is_not_a_version() {
        let txs = parse_apt_history(APT, &tz(0));
        let c = &txs[0].changes;
        assert_eq!((c[0].old.as_deref(), c[0].new.as_deref()), (None, Some("1.0")));
        assert_eq!((c[2].old.as_deref(), c[2].new.as_deref()), (Some("2.0"), Some("2.1")));
        assert_eq!(c.len(), 4);
    }

    #[test]
    fn apt_covers_every_action_and_records_errors_as_warnings() {
        let txs = parse_apt_history(APT, &tz(0));
        let actions: Vec<_> = txs[1].changes.iter().map(|c| c.action).collect();
        assert_eq!(actions, vec![Action::Removed, Action::Purged, Action::Downgraded, Action::Reinstalled]);
        assert_eq!(txs[1].warnings.len(), 1);
    }

    #[test]
    fn an_apt_block_cut_off_mid_line_keeps_the_complete_entries() {
        let txs = parse_apt_history(APT, &tz(0));
        assert_eq!(txs[2].changes.len(), 1);
        assert_eq!(txs[2].changes[0].name, "cut:amd64");
    }

    fn gz(text: &str) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(text.as_bytes()).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn a_rotated_gzip_file_is_read_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("apt")).unwrap();
        fs::write(dir.path().join("apt/history.log.1.gz"), gz(APT)).unwrap();
        let txs = read_history_in(dir.path(), 0, &tz(0));
        assert_eq!(txs.len(), 3);
    }

    #[test]
    fn one_unreadable_file_does_not_stop_the_others() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("dpkg.log.2.gz"), b"not gzip").unwrap();
        fs::write(dir.path().join("pacman.log"), PACMAN).unwrap();
        let txs = read_history_in(dir.path(), 0, &tz(0));
        assert_eq!(txs.len(), 3);
    }

    #[test]
    fn dpkg_runs_that_apt_also_logged_are_dropped_in_favour_of_apts_command() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("apt")).unwrap();
        fs::write(dir.path().join("apt/history.log"), APT).unwrap();
        fs::write(dir.path().join("dpkg.log"), DPKG).unwrap();
        let txs = read_history_in(dir.path(), 0, &tz(0));
        let dpkg: Vec<_> = txs.iter().filter(|t| t.manager == Manager::Dpkg).collect();
        assert_eq!(dpkg.len(), 2, "the 03-01 run lies inside an apt block; the two 03-02 12:00 runs do not");
        assert!(dpkg.iter().all(|t| t.start_us >= us("2026-03-02T12:00:00Z")));
        assert!(txs.windows(2).all(|w| w[0].start_us <= w[1].start_us));
    }

    #[test]
    fn transactions_that_ended_before_since_are_left_out() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pacman.log"), PACMAN).unwrap();
        let txs = read_history_in(dir.path(), us("2026-03-02T00:00:00Z"), &tz(0));
        assert_eq!(txs.len(), 2);
    }

    #[test]
    #[ignore = "reads this machine's /var/log/pacman.log"]
    fn real_pacman_log() {
        let text = fs::read_to_string("/var/log/pacman.log").unwrap();
        let txs = parse_pacman(&text);
        println!("{} transactions", txs.len());
        for t in txs.iter().rev().take(3).rev() {
            println!("{t:#?}");
        }
        assert!(!txs.is_empty());
    }
}
