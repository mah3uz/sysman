use std::collections::HashMap;
use std::path::Path;

use schema::helper::ProcCounters;

/// Counters of every process under `root/proc`; one that exits mid-read is skipped.
pub fn read(root: &Path, net: Option<&HashMap<u32, (u64, u64)>>) -> Vec<ProcCounters> {
    let proc = root.join("proc");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&proc).into_iter().flatten().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let dir = entry.path();
        let Some(start_ticks) = std::fs::read_to_string(dir.join("stat"))
            .ok()
            .and_then(|s| parse_start_ticks(&s))
        else {
            continue;
        };
        let (read_bytes, write_bytes) = std::fs::read_to_string(dir.join("io"))
            .map(|s| parse_io(&s))
            .unwrap_or_default();
        let fds = std::fs::read_dir(dir.join("fd")).ok().map(|d| d.count() as u64);
        let net = net.map(|n| n.get(&pid).copied().unwrap_or_default());
        out.push(ProcCounters {
            pid,
            start_ticks,
            read_bytes,
            write_bytes,
            fds,
            net_rx_bytes: net.map(|n| n.0),
            net_tx_bytes: net.map(|n| n.1),
        });
    }
    out.sort_by_key(|p| p.pid);
    out
}

/// Field 22 of `/proc/<pid>/stat`.
fn parse_start_ticks(stat: &str) -> Option<u64> {
    // comm may contain spaces and parentheses, so fields are counted from the last ")"
    let (_, tail) = stat.rsplit_once(')')?;
    tail.split_whitespace().nth(19)?.parse().ok()
}

fn parse_io(text: &str) -> (Option<u64>, Option<u64>) {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(": ")?.trim().parse().ok())
    };
    (field("read_bytes"), field("write_bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "42 (Web (Content) x) S 1 42 42 0 -1 4194560 100 0 0 0 7 3 0 0 20 0 5 0 123456 1000 50 0\n";
    const IO: &str =
        "rchar: 10\nwchar: 20\nsyscr: 1\nsyscw: 2\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n";

    #[test]
    fn start_ticks_survive_a_comm_with_spaces_and_parentheses() {
        // with the wrong field the instance key changes and every rate restarts
        assert_eq!(parse_start_ticks(STAT), Some(123456));
        assert_eq!(parse_start_ticks("42 (x) S 1"), None);
    }

    #[test]
    fn io_takes_storage_bytes_not_rchar() {
        // rchar counts reads served from the page cache, which is not disk traffic
        assert_eq!(parse_io(IO), (Some(4096), Some(8192)));
        assert_eq!(parse_io("rchar: 10\n"), (None, None));
    }

    #[test]
    fn reads_every_pid_and_leaves_unreadable_fields_absent() {
        let root = tempfile::tempdir().unwrap();
        let p = root.path().join("proc");
        for d in ["42/fd", "7"] {
            std::fs::create_dir_all(p.join(d)).unwrap();
        }
        std::fs::create_dir_all(p.join("self")).unwrap();
        std::fs::write(p.join("42/stat"), STAT).unwrap();
        std::fs::write(p.join("42/io"), IO).unwrap();
        std::fs::write(p.join("42/fd/0"), "").unwrap();
        std::fs::write(p.join("42/fd/1"), "").unwrap();
        std::fs::write(p.join("7/stat"), STAT.replacen("42 ", "7 ", 1)).unwrap();
        let net = HashMap::from([(42, (100, 200))]);

        let got = read(root.path(), Some(&net));
        assert_eq!(got.len(), 2, "non-numeric entries are not processes");
        let p42 = &got[1];
        assert_eq!((p42.pid, p42.start_ticks, p42.fds), (42, 123456, Some(2)));
        assert_eq!((p42.read_bytes, p42.write_bytes), (Some(4096), Some(8192)));
        assert_eq!((p42.net_rx_bytes, p42.net_tx_bytes), (Some(100), Some(200)));
        let p7 = &got[0];
        assert_eq!((p7.read_bytes, p7.fds), (None, None));
        // with the network source on, a process that sent nothing has zero, not unknown
        assert_eq!(p7.net_rx_bytes, Some(0));
        assert_eq!(read(root.path(), None)[1].net_rx_bytes, None);
    }
}
