use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use schema::helper::{ProcCounters, ProcSockets};

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

/// The listening sockets each process under `root/proc` holds, as inodes; one that exits or is replaced mid-read is
/// skipped. Connections are left out: joined with the world-readable tables they would tie every user's peers to
/// processes. Fails rather than answer unfiltered when the host's socket tables cannot be read.
pub fn sockets(root: &Path) -> Result<Vec<ProcSockets>, String> {
    // PID 1's tables, since PrivateNetwork gives the helper an empty network namespace of its own
    let listening: HashSet<u64> = schema::listeners::read_net(&root.join("proc/1/net"))
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|l| l.inode)
        .filter(|&n| n != 0)
        .collect();
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root.join("proc")).into_iter().flatten().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let dir = entry.path();
        let start = || {
            std::fs::read_to_string(dir.join("stat"))
                .ok()
                .and_then(|s| parse_start_ticks(&s))
        };
        let Some(start_ticks) = start() else {
            continue;
        };
        let mut inodes = socket_inodes(&dir.join("fd"));
        inodes.retain(|n| listening.contains(n));
        // links are read by path, so a pid reused during the read would lend its sockets to the old instance
        if inodes.is_empty() || start() != Some(start_ticks) {
            continue;
        }
        out.push(ProcSockets {
            pid,
            start_ticks,
            inodes,
        });
    }
    out.sort_by_key(|p| p.pid);
    Ok(out)
}

fn socket_inodes(fd_dir: &Path) -> Vec<u64> {
    let mut inodes: Vec<u64> = std::fs::read_dir(fd_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| socket_inode(std::fs::read_link(e.path()).ok()?.as_os_str().as_bytes()))
        .collect();
    inodes.sort_unstable();
    inodes.dedup();
    inodes
}

/// `N` from an fd link target `socket:[N]`.
fn socket_inode(target: &[u8]) -> Option<u64> {
    let digits = target.strip_prefix(b"socket:[")?.strip_suffix(b"]")?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
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

    /// A `/proc/net/{tcp,udp}` row: local and remote address, state, inode.
    fn row(local: &str, remote: &str, state: &str, inode: u64) -> String {
        format!(
            "   0: {local} {remote} {state} 00000000:00000000 00:00000000 00000000  1000        0 {inode} 1 0000000000000000 100 0 0 10 0\n"
        )
    }

    /// PID 1's tables hold TCP listener 1001, TCP connection 2002, unconnected UDP 3003 and connected UDP 4004.
    fn host_tables(root: &Path) {
        let net = root.join("proc/1/net");
        std::fs::create_dir_all(&net).unwrap();
        let head = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
        std::fs::write(
            net.join("tcp"),
            format!(
                "{head}{}{}",
                row("00000000:0016", "00000000:0000", "0A", 1001),
                row("0F02000A:D2F0", "2A01A8C0:01BB", "01", 2002)
            ),
        )
        .unwrap();
        std::fs::write(
            net.join("udp"),
            format!(
                "{head}{}{}",
                row("3500007F:0035", "00000000:0000", "07", 3003),
                row("0F02000A:9C40", "08080808:0035", "01", 4004)
            ),
        )
        .unwrap();
    }

    #[test]
    fn sockets_lists_only_listening_socket_inodes_of_processes_holding_any() {
        // other fd targets are file paths, and connections joined with /proc/net/tcp name every user's peers
        let root = tempfile::tempdir().unwrap();
        host_tables(root.path());
        let p = root.path().join("proc");
        for d in ["42/fd", "7/fd", "8"] {
            std::fs::create_dir_all(p.join(d)).unwrap();
        }
        std::fs::write(p.join("42/stat"), STAT).unwrap();
        std::fs::write(p.join("7/stat"), STAT.replacen("42 ", "7 ", 1)).unwrap();
        std::fs::write(p.join("8/stat"), STAT.replacen("42 ", "8 ", 1)).unwrap();
        let link = |pid: &str, fd: &str, target: &str| {
            std::os::unix::fs::symlink(target, p.join(format!("{pid}/fd/{fd}"))).unwrap();
        };
        link("42", "3", "socket:[1001]");
        link("42", "4", "/home/someone/secret.txt");
        link("42", "5", "pipe:[77]");
        link("42", "6", "anon_inode:[eventfd]");
        link("42", "7", "socket:[1001]");
        link("42", "8", "socket:[2002]");
        link("42", "9", "socket:[]");
        link("42", "10", "socket:[3003]");
        link("42", "11", "socket:[4004]");
        // a unix socket is in none of the tables
        link("42", "12", "socket:[5005]");
        link("7", "0", "/dev/null");
        link("7", "1", "socket:[2002]");
        link("7", "2", "socket:[5005]");

        assert_eq!(
            sockets(root.path()).unwrap(),
            vec![ProcSockets {
                pid: 42,
                start_ticks: 123456,
                inodes: vec![1001, 3003],
            }],
            "a duplicated fd is one socket; processes with no listening socket or no readable fd are left out"
        );
    }

    #[test]
    fn sockets_fails_rather_than_answer_unfiltered_without_the_host_tables() {
        // with no tables to filter by, the only other answer would be every socket of every process
        let root = tempfile::tempdir().unwrap();
        let p = root.path().join("proc/42");
        std::fs::create_dir_all(p.join("fd")).unwrap();
        std::fs::write(p.join("stat"), STAT).unwrap();
        std::os::unix::fs::symlink("socket:[2002]", p.join("fd/3")).unwrap();
        let missing = sockets(root.path()).unwrap_err();
        assert!(missing.contains("proc/1/net"), "{missing}");

        host_tables(root.path());
        let tcp = root.path().join("proc/1/net/tcp");
        std::fs::remove_file(&tcp).unwrap();
        // a directory opens but cannot be read, like a table this process is refused, even when tests run as root
        std::fs::create_dir(&tcp).unwrap();
        let unreadable = sockets(root.path()).unwrap_err();
        assert!(unreadable.contains("proc/1/net/tcp"), "{unreadable}");
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
