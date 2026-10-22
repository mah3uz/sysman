use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Result, bail};
use schema::listeners::{Holder, Listener, Listeners};

use crate::sys::Sys;

type ByInode = HashMap<u64, Vec<Holder>>;

/// Every listening TCP and bound, unconnected UDP socket in the daemon's network namespace, each with the process
/// instances holding it where they can be found.
pub fn scan(sys: &Sys) -> Result<Listeners> {
    let mut found = schema::listeners::read_net(&sys.path("/proc/net"))?;
    let (holders, holders_error) = holders(sys, &found)?;
    for l in &mut found {
        if let Some(h) = holders.get(&l.inode) {
            l.holders = h.clone();
        }
    }
    found.sort_by_key(|l| (l.port, l.protocol, l.address));
    Ok(Listeners {
        listeners: found,
        holders_error,
    })
}

/// Socket inode → the process instances holding it, from the fd links this user can read and from the helper,
/// and why the helper's part is missing when it is.
fn holders(sys: &Sys, listeners: &[Listener]) -> Result<(ByInode, Option<String>)> {
    // inode 0 is a socket the kernel opened for itself, which no fd links to
    let wanted: HashSet<u64> = listeners.iter().map(|l| l.inode).filter(|&n| n != 0).collect();
    let mut found = ByInode::new();
    if wanted.is_empty() {
        return Ok((found, None));
    }
    let (btime, clk_tck) = clock(sys)?;
    let holder = |pid, start_ticks: u64, comm: &str| Holder {
        pid,
        start_time: btime + start_ticks / clk_tck,
        comm: comm.to_string(),
    };

    for pid in sys.list("/proc") {
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        // other users' fd directories are refused here; those are what the helper is asked for
        let Ok(fds) = std::fs::read_dir(sys.path(format!("/proc/{pid}/fd"))) else {
            continue;
        };
        let Some((comm, start)) = identity(sys, pid) else {
            continue;
        };
        let hits: Vec<u64> = fds
            .flatten()
            .filter_map(|e| socket_inode(&std::fs::read_link(e.path()).ok()?))
            .filter(|n| wanted.contains(n))
            .collect();
        // links are read by path, so a pid reused during the read would lend its sockets to the old instance
        if hits.is_empty() || identity(sys, pid).map(|i| i.1) != Some(start) {
            continue;
        }
        for n in hits {
            found.entry(n).or_default().push(holder(pid, start, &comm));
        }
    }

    // matched against every inode, not only the unmatched: a socket shared with another user's process has both
    let holders_error = match sys.helper().sockets() {
        Ok(answer) => {
            for p in &answer.processes {
                let hits: Vec<u64> = p.inodes.iter().copied().filter(|n| wanted.contains(n)).collect();
                if hits.is_empty() {
                    continue;
                }
                // the pid may have been reused since the helper read it
                let Some((comm, _)) = identity(sys, p.pid).filter(|i| i.1 == p.start_ticks) else {
                    continue;
                };
                for n in hits {
                    found.entry(n).or_default().push(holder(p.pid, p.start_ticks, &comm));
                }
            }
            None
        }
        Err(e) => Some(e),
    };
    for h in found.values_mut() {
        h.sort_by_key(|h| h.pid);
        h.dedup_by_key(|h| h.pid);
    }
    Ok((found, holders_error))
}

/// Boot time in Unix seconds and clock ticks per second, to turn `start_ticks` into `start_time` as the process
/// table does.
fn clock(sys: &Sys) -> Result<(u64, u64)> {
    // SAFETY: sysconf has no preconditions.
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if clk_tck <= 0 {
        bail!("sysconf could not report the clock tick rate");
    }
    let btime = sys
        .read_opt("/proc/stat")
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok()))
        .unwrap_or(0);
    Ok((btime, clk_tck as u64))
}

/// `comm` and `start_ticks` (field 22) from `/proc/<pid>/stat`; `None` once the process is gone.
fn identity(sys: &Sys, pid: u32) -> Option<(String, u64)> {
    let stat = sys.read_opt(format!("/proc/{pid}/stat"))?;
    // comm may contain spaces and parentheses, so split on the last ")"
    let (head, tail) = stat.rsplit_once(')')?;
    let comm = head.split_once('(')?.1.to_string();
    Some((comm, tail.split_whitespace().nth(19)?.parse().ok()?))
}

/// `N` from an fd link target `socket:[N]`.
fn socket_inode(target: &Path) -> Option<u64> {
    let digits = target
        .as_os_str()
        .as_bytes()
        .strip_prefix(b"socket:[")?
        .strip_suffix(b"]")?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::os::unix::fs::symlink;

    use schema::listeners::Protocol;
    use serde_json::json;

    use super::*;
    use crate::testutil::Fixture;

    const HEAD: &str =
        "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

    /// A `/proc/net` row in the kernel's layout.
    fn row(local: &str, remote: &str, state: &str, uid: u32, inode: u64) -> String {
        format!(
            "   0: {local} {remote} {state} 00000000:00000000 00:00000000 00000000 {uid:>5}        0 {inode} 1 0000000000000000 100 0 0 10 0\n"
        )
    }

    fn table(rows: &[String]) -> String {
        format!("{HEAD}{}", rows.concat())
    }

    /// (protocol, address, port, uid, inode) of each listener.
    fn short(ls: &[Listener]) -> Vec<(Protocol, IpAddr, u16, u32, u64)> {
        ls.iter()
            .map(|l| (l.protocol, l.address, l.port, l.uid, l.inode))
            .collect()
    }

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(a, b, c, d))
    }

    #[test]
    fn tcp_lists_listening_sockets_and_not_connections() {
        // an established connection's local port is not a service anyone can reach
        let f = Fixture::new().file(
            "proc/net/tcp",
            &table(&[
                row("0100007F:0277", "00000000:0000", "0A", 0, 1001),
                row("00000000:0016", "00000000:0000", "0A", 0, 1002),
                row("0F02000A:D2F0", "2A01A8C0:01BB", "01", 1000, 1003),
                row("0F02000A:0016", "0B02000A:C350", "06", 0, 0),
            ]),
        );
        assert_eq!(
            short(&scan(&f.sys()).unwrap().listeners),
            vec![
                (Protocol::Tcp, v4(0, 0, 0, 0), 22, 0, 1002),
                (Protocol::Tcp, v4(127, 0, 0, 1), 631, 0, 1001),
            ]
        );
    }

    #[test]
    fn ipv6_addresses_decode_and_v4_mapped_ones_read_as_ipv4() {
        // the app judges "this machine only" by the address; a mapped 127.0.0.1 must look like loopback
        let f = Fixture::new().file(
            "proc/net/tcp6",
            &table(&[
                row(
                    "00000000000000000000000000000000:0016",
                    "00000000000000000000000000000000:0000",
                    "0A",
                    0,
                    1,
                ),
                row(
                    "00000000000000000000000001000000:0019",
                    "00000000000000000000000000000000:0000",
                    "0A",
                    0,
                    2,
                ),
                row(
                    "0000000000000000FFFF00000100007F:1F90",
                    "00000000000000000000000000000000:0000",
                    "0A",
                    1000,
                    3,
                ),
                row(
                    "B80D0120000000000000000001000000:01BB",
                    "00000000000000000000000000000000:0000",
                    "0A",
                    33,
                    4,
                ),
                row(
                    "000080FE00000000FF005452120034FE:0016",
                    "000080FE00000000FF005452120035FE:C350",
                    "01",
                    0,
                    5,
                ),
            ]),
        );
        let got = scan(&f.sys()).unwrap().listeners;
        assert_eq!(
            short(&got),
            vec![
                (Protocol::Tcp6, IpAddr::V6(Ipv6Addr::UNSPECIFIED), 22, 0, 1),
                (Protocol::Tcp6, IpAddr::V6(Ipv6Addr::LOCALHOST), 25, 0, 2),
                (Protocol::Tcp6, "2001:db8::1".parse().unwrap(), 443, 33, 4),
                (Protocol::Tcp6, v4(127, 0, 0, 1), 8080, 1000, 3),
            ]
        );
        assert!(got.iter().all(|l| l.holders.is_empty()));
    }

    #[test]
    fn udp_lists_bound_unconnected_sockets_and_not_connected_ones() {
        // a connected UDP socket talks to one peer, like a TCP connection; only unconnected ones receive from anyone
        let zero6 = "00000000000000000000000000000000:0000";
        let f = Fixture::new()
            .file(
                "proc/net/udp",
                &table(&[
                    row("3500007F:0035", "00000000:0000", "07", 991, 10),
                    row("0F02000A:9C40", "08080808:0035", "01", 1000, 11),
                ]),
            )
            .file(
                "proc/net/udp6",
                &table(&[row("00000000000000000000000000000000:14E9", zero6, "07", 1000, 12)]),
            );
        assert_eq!(
            short(&scan(&f.sys()).unwrap().listeners),
            vec![
                (Protocol::Udp, v4(127, 0, 0, 53), 53, 991, 10),
                (Protocol::Udp6, IpAddr::V6(Ipv6Addr::UNSPECIFIED), 5353, 1000, 12),
            ]
        );
    }

    #[test]
    fn a_row_repeated_across_reads_is_listed_once_and_kernel_sockets_each_once() {
        // each read of a table is filled on its own, so a row can repeat; inode 0 alone cannot tell kernel sockets apart
        let f = Fixture::new().file(
            "proc/net/tcp",
            &table(&[
                row("00000000:0016", "00000000:0000", "0A", 0, 1002),
                row("00000000:0016", "00000000:0000", "0A", 0, 1002),
                row("00000000:0801", "00000000:0000", "0A", 0, 0),
                row("00000000:0801", "00000000:0000", "0A", 0, 0),
                row("00000000:4E2D", "00000000:0000", "0A", 0, 0),
            ]),
        );
        assert_eq!(
            short(&scan(&f.sys()).unwrap().listeners),
            vec![
                (Protocol::Tcp, v4(0, 0, 0, 0), 22, 0, 1002),
                (Protocol::Tcp, v4(0, 0, 0, 0), 2049, 0, 0),
                (Protocol::Tcp, v4(0, 0, 0, 0), 20013, 0, 0),
            ]
        );
    }

    #[test]
    fn no_socket_tables_at_all_is_an_error_not_an_empty_list() {
        let err = scan(&Fixture::new().sys()).unwrap_err().to_string();
        assert!(err.contains("no socket tables"), "{err}");
    }

    fn stat(pid: u32, comm: &str, start_ticks: u64) -> String {
        format!("{pid} ({comm}) S 1 {pid} {pid} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {start_ticks} 1 1 0\n")
    }

    /// pid 100 is this user's (fd links readable); pids 1, 200 and 300 are other users' (no readable fd directory).
    fn host() -> Fixture {
        let f = Fixture::new()
            .file("proc/stat", "cpu  1 2 3\nbtime 1700000000\n")
            .file(
                "proc/net/tcp",
                &table(&[
                    row("0100007F:1F90", "00000000:0000", "0A", 1000, 1001),
                    row("00000000:0016", "00000000:0000", "0A", 0, 2002),
                    row("00000000:0050", "00000000:0000", "0A", 33, 3003),
                    row("00000000:0801", "00000000:0000", "0A", 0, 0),
                ]),
            )
            .file("proc/100/stat", &stat(100, "dev server", 25_000))
            .file("proc/1/stat", &stat(1, "systemd", 1))
            .file("proc/200/stat", &stat(200, "sshd", 500))
            .file("proc/300/stat", &stat(300, "reused", 900));
        let fd = f.path().join("proc/100/fd");
        std::fs::create_dir_all(&fd).unwrap();
        symlink("socket:[1001]", fd.join("3")).unwrap();
        symlink("/home/me/notes.txt", fd.join("4")).unwrap();
        symlink("socket:[5555]", fd.join("5")).unwrap();
        f
    }

    fn holders_of(ls: &[Listener], port: u16) -> Vec<(u32, u64, String)> {
        let l = ls.iter().find(|l| l.port == port).unwrap();
        l.holders
            .iter()
            .map(|h| (h.pid, h.start_time, h.comm.clone()))
            .collect()
    }

    fn pids_of(ls: &[Listener], port: u16) -> Vec<u32> {
        holders_of(ls, port).into_iter().map(|h| h.0).collect()
    }

    #[test]
    fn holders_come_from_readable_fd_links_then_the_helper_matched_by_pid_and_start() {
        let f = host();
        f.fake_helper(|line| {
            assert_eq!(
                line, "{\"cmd\":\"sockets\"}\n",
                "the helper is asked for nothing but sockets"
            );
            json!({"processes": [
                // a socket unit's listener is held by PID 1 and its service alike
                {"pid": 1, "start_ticks": 1, "inodes": [2002, 7]},
                {"pid": 200, "start_ticks": 500, "inodes": [2002]},
                // pid 300 has started anew since the helper read it: its sockets belonged to the old instance
                {"pid": 300, "start_ticks": 800, "inodes": [3003]},
                {"pid": 400, "start_ticks": 1, "inodes": [3003]}
            ]})
        });
        let sys = f.sys();
        let (btime, tck) = clock(&sys).unwrap();
        assert_eq!(btime, 1_700_000_000);
        let got = scan(&sys).unwrap();
        assert_eq!(got.holders_error, None);
        let got = got.listeners;
        assert_eq!(
            holders_of(&got, 8080),
            vec![(100, btime + 25_000 / tck, "dev server".to_string())],
            "start_time matches the process table's, so the app can find the row"
        );
        assert_eq!(
            holders_of(&got, 22),
            vec![
                (1, btime + 1 / tck, "systemd".to_string()),
                (200, btime + 500 / tck, "sshd".to_string())
            ]
        );
        let http = got.iter().find(|l| l.port == 80).unwrap();
        assert!(
            http.holders.is_empty(),
            "a reused pid and an exited one are not holders"
        );
        assert_eq!(http.uid, 33, "the socket still names its owner");
    }

    #[test]
    fn a_socket_shared_with_another_users_process_lists_both_holders() {
        // a server started by this user that hands its socket to a privileged worker, or the other way round
        let f = host();
        f.fake_helper(|_| {
            json!({"processes": [
                {"pid": 100, "start_ticks": 25_000, "inodes": [1001]},
                {"pid": 200, "start_ticks": 500, "inodes": [1001]}
            ]})
        });
        let got = scan(&f.sys()).unwrap().listeners;
        assert_eq!(pids_of(&got, 8080), vec![100, 200], "each holder once");
    }

    #[test]
    fn a_kernel_socket_has_no_holder() {
        // inode 0 is every socket the kernel opened itself; a process claiming it would be named for all of them
        let f = host();
        symlink("socket:[0]", f.path().join("proc/100/fd/6")).unwrap();
        f.fake_helper(|_| json!({"processes": [{"pid": 200, "start_ticks": 500, "inodes": [0, 2002]}]}));
        let got = scan(&f.sys()).unwrap().listeners;
        assert!(pids_of(&got, 2049).is_empty());
        assert_eq!(pids_of(&got, 22), vec![200]);
    }

    #[test]
    fn without_the_helper_other_users_sockets_keep_only_their_uid_and_say_why() {
        let f = host();
        let got = scan(&f.sys()).unwrap();
        assert_eq!(got.holders_error.as_deref(), Some("sysman-helper is not running"));
        assert_eq!(
            holders_of(&got.listeners, 8080).len(),
            1,
            "this user's own processes are still found"
        );
        let ssh = got.listeners.iter().find(|l| l.port == 22).unwrap();
        assert_eq!((ssh.uid, ssh.holders.len()), (0, 0));
    }

    #[test]
    fn a_helper_that_cannot_list_sockets_leaves_the_answer_as_without_it_and_says_why() {
        for (answer, why) in [
            (
                json!({"error": "expected {\"cmd\":\"status\"} or {\"cmd\":\"read\"}"}),
                "older than sysmand",
            ),
            (
                json!({"error": "reading /proc/1/net/tcp: Permission denied (os error 13)"}),
                "sysman-helper: reading /proc/1/net/tcp: Permission denied",
            ),
        ] {
            let f = host();
            f.fake_helper(move |_| answer.clone());
            let got = scan(&f.sys()).unwrap();
            assert_eq!(got.listeners.len(), 4);
            assert!(pids_of(&got.listeners, 22).is_empty());
            let error = got.holders_error.unwrap();
            assert!(error.contains(why), "{error}");
        }
    }

    #[test]
    fn a_helper_socket_this_user_may_not_open_says_so() {
        // the daemon's user is not in the helper's group
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: root ignores the socket's mode");
            return;
        }
        let f = host();
        let path = f.path().join(crate::helper::SOCKET.trim_start_matches('/'));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        let error = scan(&f.sys()).unwrap().holders_error.unwrap();
        assert!(error.contains("Permission denied"), "{error}");
    }

    #[test]
    fn socket_links_parse_only_as_socket_inodes() {
        assert_eq!(socket_inode(Path::new("socket:[]")), None);
        assert_eq!(socket_inode(Path::new("socket:[1x]")), None);
        assert_eq!(socket_inode(Path::new("pipe:[12]")), None);
        assert_eq!(socket_inode(Path::new("socket:[12]")), Some(12));
    }
}
