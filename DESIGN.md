# sysman — design

A Linux system monitor: a collector daemon (`sysmand`) that records metrics, and a
GPUI app (`sysman`) that shows them live and as history.

## Decisions

| Topic | Decision |
| --- | --- |
| Order | Collector first, then the app. |
| History | SQLite, plus a small fsync'd JSONL file kept as crash evidence. |
| Live data | The daemon serves it over a Unix socket. |
| Paths | XDG directories. |
| DB model | Numeric series with rollups; list-shaped data as JSON snapshots. |
| Processes | Every process recorded every 10 s (48 h raw, 7 days of 1-minute rollups); top N in the crash log; the full table also on the socket. |
| JSONL | Today and yesterday, uncompressed. |
| NVIDIA | NVML in-process; after an NVML timeout, `nvidia-smi` with a kill timeout until restart. |
| Hardware | Vendor-neutral records. GPUs: NVIDIA, AMD, Intel (i915 and xe), generic DRM fallback. CPUs: x86 (AMD, Intel). |
| Verification | This machine for AMD CPU, NVIDIA and amdgpu. Fixtures built from kernel docs for the rest, marked unverified on hardware. |
| Sensors | Super I/O chips (nct67xx) read every 10 s, everything else every 2 s. |
| App design | `docs/APP-DESIGN.md`. v1 has every screen, Disk Map included. |
| Alerts | Evaluated in `sysmand` (so they fire with the app closed), freedesktop notifications; after the app's core screens. |

## Non-negotiables

1. **The last reading before a crash is on disk.** Every JSONL line is written
   and `fdatasync`'d before the next tick.
2. **One failing source does not stop the others.** Every collector runs on its
   own thread; a tick never waits on a collector past its deadline.

## Layout

```
Cargo.toml                 workspace
crates/schema              record types (serde), shared by daemon and app
crates/config              config.toml types, paths, and comment-preserving edits, shared by daemon and app
crates/journal             systemd journal reader, log categories and findings, package history parsers
crates/sysmand             daemon: config, collectors, scheduler, JSONL, SQLite, socket
crates/sysman-helper       optional privileged helper: RAPL, /proc I/O, SMART, eBPF network
crates/sysman              GPUI app
dist/sysmand.service       systemd user unit
dist/sysman-helper.service systemd system unit for the helper (not enabled)
dist/arch, dist/deb        Arch and Debian packaging; dist/install.sh for other distributions
```

| What | Path |
| --- | --- |
| Config | `$XDG_CONFIG_HOME/sysman/config.toml` (default `~/.config/sysman/config.toml`) |
| History | `$XDG_DATA_HOME/sysman/history.db` (default `~/.local/share/sysman/`) |
| Crash log | `$XDG_DATA_HOME/sysman/log/YYYY-MM-DD.jsonl` |
| Socket | `$XDG_RUNTIME_DIR/sysman.sock` |
| Service | `~/.config/systemd/user/sysmand.service` (installed from `dist/`) |

## Daemon

### Collectors

```rust
trait Collector: Send {
    fn meta(&mut self) -> Option<Value> { None }
    fn sample(&mut self) -> anyhow::Result<Option<Value>>;
}
```

Modules are registered in `MODULES` (`collector.rs`) with a default interval,
whether the database keeps a JSON snapshot of them, and which of their fields
stay out of the crash log. All `/proc` and `/sys` reads go through `Sys`, whose
root points at a fixture directory in tests. A constructor error disables the
module and is listed in the meta record's `failed_modules`. One bad entity (a
malformed process, a vanished device) is skipped, never a module failure.

| Module | Interval | Snapshot | Notes |
| --- | --- | --- | --- |
| system, cpu, memory, pressure, hwmon, gpu, disk, net | 2 s | | |
| processes, cgroups | 10 s | yes | `processes.table` (all rows) is not logged to JSONL |
| helper | 10 s | | |
| fs | 30 s | | |
| netinfo, storage, power | 60 s | yes | |

### Scheduling and isolation

- Ticks land on multiples of the base interval on the wall clock;
  `tick = round(now / base)`; a module runs when `tick % every == 0`.
- The first tick after a start samples every module, so slow modules are
  present immediately (in the log after every reboot, and on the socket).
- Each module owns a worker thread. The scheduler collects replies until all
  arrive or the deadline (`0.75 × base`) passes. A module past the deadline gets
  `errors.<name> = "timeout…"`; while its previous request is still running it is
  not sent another and gets `"busy: still running since …"`. Late replies are
  discarded. Panics are caught and recorded as errors.
- At startup every collector is sampled once and the result discarded, to prime
  rate counters.

### GPU isolation

NVML runs on an inner thread with a 1.0 s deadline (inside the tick deadline,
so the switch is recorded). On the first timeout the thread is abandoned and
`gpu.backend_errors.<pci>` reports the switch; `nvidia-smi` is used from then on
as a child that is killed and never waited on after 1.0 s, with at most one
outstanding. A runtime-suspended GPU is not read, so polling never wakes it.

### Sensor cost

The nct67xx Super I/O driver re-reads every register over port I/O whenever its
cache is older than ~1.5 s; at 2 s polling that alone cost ~4.6% of a core.
`[modules.hwmon] chip_intervals` maps chip-name prefixes to seconds (longest
match wins); the default is `{ nct67 = 10 }`. Measured whole-daemon cost with
defaults: ~2% of one core.

### Writers

1. **JSONL** (scheduler thread): compact JSON, `write` + `fdatasync` per line.
   A meta line at startup and at local midnight. Files older than
   `retention.log_days` are deleted at rollover. ~290 KB/minute.
2. **SQLite** (own thread, bounded queue; when full, records are dropped and
   counted, so the database never delays the crash log). WAL,
   `synchronous=NORMAL`, one transaction per record.

### Database

```sql
series     (id, path UNIQUE)
points     (series_id, t, value)                         PK (series_id, t)
rollup_1m  (series_id, t, min, avg, max, count)          PK (series_id, t)
rollup_15m (same)
snapshots  (module, t, data)                             PK (module, t)
errors     (t, module, message)                          PK (t, module)
meta       (t, data)
procs      (id, pid, start_time, comm, cmdline, user, first_t, last_t)  UNIQUE (pid, start_time)
proc_points    (proc_id, t, cpu_pct, rss_bytes, threads, fds, read_bps, write_bps)
proc_rollup_1m (proc_id, t, cpu_avg, cpu_max, rss_avg, rss_max, threads_max, fds_max, read_bps_avg, write_bps_avg, count)
events     (t, kind, key, boot_id, summary, data)        PK (t, kind, key)
events_state (name PRIMARY KEY, value)
```

- `t` is integer Unix seconds.
- **Series** are the numeric (and boolean) leaves of a sample, named by JSON
  pointer (RFC 6901), e.g. `/hwmon/k10temp~10000:00:18.3/temp/Tctl`. Leaves under
  an array of objects are not series (a ranked list's index has no identity), so
  devices are keyed by stable id in maps, never in arrays. `null` is skipped.
- **Rollups**: 1-minute buckets accumulate in memory and are written when the
  minute closes, then merged into their 15-minute bucket with an upsert that
  combines min/max/avg/count. On startup, minutes a previous run left open are
  rebuilt from raw points, so a restart loses nothing.
- **Processes**: one `procs` row per process instance (pid + start time, so a
  reused pid is a new process), one `proc_points` row per process per 10 s.
- **Retention** runs hourly as per-key range deletes on primary keys.

| Data | Default retention | Key |
| --- | --- | --- |
| `points`, `snapshots`, `errors`, `proc_points` | 48 h | `raw_hours` |
| `rollup_1m` | 90 days | `rollup_1m_days` |
| `rollup_15m` | 730 days | `rollup_15m_days` |
| `proc_rollup_1m`, `procs` | 7 days | `proc_rollup_days` |
| `events` | 90 days | `events_days` |
| JSONL | 2 days | `log_days` |
| `meta`, `events_state` | forever | |

### Socket

Line-delimited JSON over `$XDG_RUNTIME_DIR/sysman.sock`, mode 0600. One request
per line, one response per line; errors are `{"error": "…"}`.

| Request | Response |
| --- | --- |
| `{"cmd":"meta"}` | latest meta record |
| `{"cmd":"latest"}` | latest sample's envelope plus each module's most recent value (includes `processes.table`) |
| `{"cmd":"subscribe"}` | meta, then every record as produced; a client 32 lines behind is disconnected |
| `{"cmd":"processes"}` | full process table now, CPU relative to this client's previous request (first answer takes ~0.5 s) |
| `{"cmd":"listeners"}` | every listening TCP and bound UDP socket now: protocol, local address and port, owning uid, and the holding processes (pid, start time, comm) where known |

`listeners` is answered on request only, never per tick. Sockets come from
`/proc/net/{tcp,tcp6,udp,udp6}` (any user may read them): TCP in LISTEN, UDP
bound to a port and unconnected; a v4-mapped IPv6 address is given as IPv4; a
row the kernel repeats across reads of a table is listed once. Unconnected UDP
includes clients' ephemeral sockets (DNS lookups, QUIC, mDNS), as `ss -uln`
does, so a UDP port is not by itself a service others reach. The holder is
found by matching socket inodes in `/proc/<pid>/fd`, which the daemon can read
for its own user's processes, and in the helper's `sockets` answer for all
listed inodes, so a socket shared with another user's process names both.
`sockets` waits up to 3 s; its answer is shared for 1 s by every client and
only one request is in flight, so clients asking in a loop cost the helper one
walk a second. A socket has every process instance holding it (`holders`, by
pid; workers, or PID 1 or a user `systemd` for a socket unit beside its
service); a holder is read from its `stat` before and after its links, and a
helper answer is kept only while the pid's `start_ticks` still match, so a
reused pid is never named. A socket with no known holder keeps its uid. A
socket the kernel opened itself (NFS, WireGuard) has inode 0 and never a
holder. With `/proc` mounted `hidepid=2` the daemon cannot read other users'
`stat`, so their holders are dropped even with the helper. When the helper's
part is missing, `holders_error` says why: not running, permission denied on
its socket, too old to know `sockets`, no answer in time, or its own error
(such as `/proc/1/net` unreadable). The reply is
`{"type":"listeners","t":…,"listeners":[{"protocol":"tcp","address":"127.0.0.1","port":631,"uid":0,"inode":…,"holders":[{"pid":…,"start_time":…,"comm":"cupsd"}]}],"holders_error":"sysman-helper is not running"}`
(`schema::listeners::Listeners`, less `type` and `t`). Only the daemon's
network namespace is listed: ports bound inside a container's own namespace
are not.

A second daemon is refused when the socket answers; any other socket failure
leaves the daemon logging without it. History is not served over the socket:
the app opens `history.db` read-only (WAL allows concurrent readers).

## Privileged helper

Some data needs root or a capability: SMART (raw drive commands), CPU package
power (RAPL `energy_uj` is root-only since the PLATYPUS side channel,
CVE-2020-8694), other users' `/proc/<pid>/io` and `/proc/<pid>/fd`, and
per-process network traffic (Linux keeps no such counter; eBPF does).
`sysmand` stays unprivileged; a separate, optional `sysman-helper` reads only
these and nothing else.

- **Off by default.** Installed as `/usr/lib/systemd/system/sysman-helper.service`
  and not enabled. The app's Settings window turns it on and off with
  `systemctl enable --now` / `disable --now`, which asks for authentication
  through polkit. Without it everything works and the affected values stay
  "not permitted".
- **Confinement.** Runs as root, limited by systemd to
  `CAP_SYS_ADMIN CAP_SYS_RAWIO CAP_DAC_READ_SEARCH CAP_SYS_PTRACE CAP_BPF
  CAP_PERFMON`, with `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`,
  `PrivateNetwork`, `RestrictAddressFamilies=AF_UNIX`, `ProtectKernelTunables`,
  `ProtectKernelModules`, `MemoryDenyWriteExecute` and a `@system-service`
  syscall filter plus `bpf`, no writable paths except its runtime directory.
  Device nodes stay visible (no `PrivateDevices`/`DevicePolicy`) because SMART
  opens them. It runs a fixed set of read-only queries; it never executes
  anything a client sends (the only child is `smartctl` with fixed arguments).
- **Access.** Unix socket `/run/sysman-helper/helper.sock`, mode 0660, group
  `wheel` on Arch; under `install.sh` whichever of `wheel` and `sudo` exists
  (`wheel` first); `sudo` in the .deb. The group is given as `--group` and as
  the unit's `Group=`, which must match, so the socket is created with that
  group and no `CAP_CHOWN` is needed. Only administrators, who could
  read this data as root anyway. Each connection gets at most one answer per
  second (later requests wait), at most 8 connections are served at once, and
  a request line is at most 256 bytes.
- **Protocol.** Line-delimited JSON, one request per line:
  `{"cmd":"status"}` → version, `features` (`rapl`, `proc_io`, `smart`, `net`
  → works) and `errors` (feature → why it is off or partial);
  `{"cmd":"read"}` → RAPL energy counters (µJ, per powercap zone, with
  `max_energy_range_uj`), per-process cumulative counters for every pid
  (`read_bytes`, `write_bytes`, `fds`, eBPF `net_rx_bytes`, `net_tx_bytes`,
  keyed by pid and `start_ticks`, field 22 of `stat`), and the latest SMART
  readings by block device;
  `{"cmd":"sockets"}` → for every process holding a listening socket (pid and
  `start_ticks`), the inodes of its `socket:[N]` links in `/proc/<pid>/fd`
  whose socket is listening TCP or bound, unconnected UDP in
  `/proc/1/net/{tcp,tcp6,udp,udp6}`, and no other fd target, so `sysmand` can
  name the holder of another user's listening socket, e.g.
  `{"processes":[{"pid":812,"start_ticks":2250,"inodes":[23817,23819]}]}`.
  Connections and unix sockets are left out: joined with the world-readable
  `/proc/net/tcp`, their inodes would tie every user's remote peers to
  processes. The tables are PID 1's because `PrivateNetwork` gives the helper
  an empty namespace of its own; when they cannot be read the answer is an
  error, never the unfiltered list. It needs the same access to other users'
  `/proc/<pid>/fd` as `fds`, so `proc_io` covers it and there is no feature
  key of its own. A field whose source is off is absent. The types are
  `schema::helper::{Request, Status, Reading, Sockets}`. Rates are computed in
  `sysmand` with its usual `Deltas`; RAPL power reuses the collector's own
  wrap-aware step.
- **Sources.** Each is optional: one that fails is `false` in `features` and
  never stops the others.
  - RAPL from `/sys/class/powercap`, read on each request.
  - `/proc/<pid>/{stat,io,fd}` for every pid, read on each request.
  - SMART every 10 minutes on its own thread. NVMe through
    `NVME_IOCTL_ADMIN_CMD` Get Log Page 0x02 (the 512-byte health log); SATA
    through `smartctl -j -n standby -H -A -i` when installed (30 s limit), so a
    drive in standby is skipped, not spun up. A runtime-suspended device is not
    opened. A skipped drive keeps its previous reading.
  - Network from a CO-RE eBPF program (`src/bpf/net.bpf.c`, skeleton generated
    by `libbpf-cargo` at build time, so building needs clang; linked against
    the system libbpf): `fexit` on `tcp_sendmsg`, `tcp_cleanup_rbuf`,
    `udp_sendmsg`, `udp_recvmsg`, `udpv6_sendmsg` and `udpv6_recvmsg` sums
    bytes per tgid in a BPF hash map. Loopback is excluded when the socket's
    destination or bound address, or a UDP message's address, is 127/8, `::1`
    or v4-mapped 127/8. A `tp_btf/sched_process_exit` program removes a
    process's entry when it exits, keeping the map bounded and a reused pid at
    zero. Kernel structs are declared by hand with `preserve_access_index`, so
    no `vmlinux.h` is needed; the kernel must have BTF. An `fexit` program must
    declare its function's exact arguments, and `udp_recvmsg`/`udpv6_recvmsg`
    changed (6 with `noblock` before 5.19, then 5, then 4 without `addr_len`),
    so the object holds one program per signature and the loader reads each
    hooked function's argument count from `/sys/kernel/btf/vmlinux` and loads
    only the matching one. Every program is optional: if the object fails to
    load, each program is loaded alone to find the ones the kernel rejects,
    and those, like any that fails to attach, are left out. `net` stays on
    while both TCP programs work, with `errors.net` naming what is not counted
    (`UDP receive not counted: …`).
- **In sysmand.** A `HelperClient`, shared through `Sys`, asks once per tick
  when the socket exists: one connection per request, a 500 ms limit, and one
  reading shared by every module of the tick. It feeds `cpu.power_w` (only
  when `energy_uj` is not readable directly), the process rows (`read_bps`,
  `write_bps` and `fds` where the scan could not read them; `net_rx_bps` and
  `net_tx_bps` for all; matched by pid and start time) and `storage.smart`.
  The socket's `processes` answer gets the same. A `helper` module (10 s, not
  snapshotted) reports `state` (`running`, `stopped`, `error`), `version`,
  `features` and `error`, so the app knows what to offer. Without the socket
  every other module's output is unchanged.
- **In the app.** Settings → Privileged data: what the helper reads and why it
  needs privileges, its state, and the toggle. Every card or column that lacks
  this data shows one line saying so and an "Enable…" button that opens
  Settings.

## Logs and events

### `crates/journal`

Shared by the app's Logs screen and the daemon's event recorder.

- **Reader.** `libsystemd.so.0` is loaded at runtime (`libloading`) with a
  hand-written table of the `sd_journal_*` functions used, so neither the
  build nor the app's start needs libsystemd. When it does not load, the
  reader runs `journalctl -o json` with the same filters (fields can arrive
  as byte arrays). An `sd_journal` handle is used by one thread at a time,
  never after a fork. API: open (system + user files), match/OR/AND, seek to
  head, tail, a time or a cursor, step, read fields (with a data threshold),
  unique values of a field, follow (`get_fd` + `process`), boots (from
  `_BOOT_ID` values, each with first and last entry).
- **Categories** are field rules: Kernel `_TRANSPORT=kernel`; Hardware kernel
  entries with `_KERNEL_SUBSYSTEM` (usb, pci, drm, nvme, scsi, ata, thermal,
  hid, …), MCE/EDAC/AER messages, and udev; Services `_SYSTEMD_UNIT`/`UNIT`;
  Apps `_SYSTEMD_USER_UNIT`/`USER_UNIT` and user-session processes;
  Scheduled cron identifiers (`crond`, `CROND`, `CRON`, `cron`, `anacron`,
  `atd`, `crontab`) and units started by a `.timer`; Sign-ins
  `SYSLOG_FACILITY` 4 or 10 plus logind session messages; Boot & power
  shutdown, sleep and boot messages; Crashes the coredump `MESSAGE_ID`.
  An entry may be in several. A missing `PRIORITY` counts as info.
- **Findings** are rules over entries (APP-DESIGN.md §6.11), each with the
  entries that make it up. MESSAGE_IDs and text patterns live in one table
  in the crate, each with a test built from a real entry.
- **Package history**: `/var/log/pacman.log` (ALPM installed, upgraded,
  removed, downgraded, reinstalled, grouped into transactions by
  `transaction started/completed`), `/var/log/dpkg.log*` and
  `/var/log/apt/history.log*`, rotated `.gz` files included. Each package
  change becomes an entry in the Packages category.
- **Access check**: opening `system.journal` under `/var/log/journal/<machine-id>`
  (or `/run/log/journal`) and getting EACCES means only the user's own entries
  are visible.

### Events in `sysmand`

An `events` thread follows the journal and writes notable entries to
`history.db`, so the app can mark them on every chart even for times the app
was closed.

```sql
events       (t, kind, key, boot_id, summary, data)      PK (t, kind, key)
events_state (name PRIMARY KEY, value)
```

- `kind`: `crash`, `oom`, `unit_failed`, `restart_loop`, `boot`,
  `boot_end`, `suspend`, `resume`, `packages`. `boot_end` data: `clean`
  (true only for a clean end), `certainty` (`clean`, `confirmed`,
  `ended_while_suspended`, `unconfirmed`), `reset_reason` (the next boot's
  AMD reset reason, named, or null) and `suspended`. A boot whose start was
  vacuumed is unknown and gets no `boot_end`, as with no journal access.
  `key` makes a re-read idempotent: the cursor of the event's first journal
  entry, the comm for `crash`, the boot id for `boot` and `boot_end`, the
  transaction's start (µs) for packages. `t` is that first entry's time (a boot's last entry for
  `boot_end`). `data` holds the fields the app shows (pid, comm, signal,
  unit, result, count, n_restarts, command and changes); `boot_id` is null
  for packages.
- A row is written as soon as its rule fires and updated in place as it
  grows: a program's crashes fold per boot (`data.count`, counting only
  crashes newer than `data.last_us`, so a re-read adds nothing), a unit's
  failures fold per boot (`data.count`), a restart loop
  counts its restarts, an OOM kill merges the lines within 5 s, a package
  transaction still running gains changes.
- The thread reads `MESSAGE_ID`s for crashes, OOM, failures, restarts and
  sleep plus kernel lines of priority 0-6 (Xid, GPU hangs and segfaults are
  warning to info), with only the fields the rules use. Unit failures,
  restarts and unit OOM kills count only from PID 1 or the daemon's own
  user manager, and sleep only from root, judged by journald's `_PID` and
  `_UID`. It starts with the daemon and resumes from `events_state`: from the
  earliest entry a still-open rule depends on (so a restart loop is neither
  split nor counted twice across a restart), else just after the last cursor
  read; on first run from `retention.events_days` back. Boots come from the
  journal's boot list; a boot's `boot_end` is decided once a later boot
  runs, from filtered reads: its logind shutdown or PID 1 stopping
  `multi-user.target` within its last 30 minutes and after its last resume
  (or journald stopping in its last minute) is clean; otherwise the next
  boot's journald "uncleanly shut down" line
  or a non-orderly reset reason confirms the abrupt end. A failure reading
  boots is listed in `errors` and the rest of the thread runs on.
  Package logs are re-read when one's mtime changes, checked every 60 s.
- Catching up uses a handle on every journal file; once caught up it
  follows on one opened on just the files written since shortly before the
  last entry read (the active ones, and an archive rotated since), because
  a filtered step maps every open file and that is hundreds of MB of RSS.
  A rotation (an active file's inode changes) reopens it before reading on,
  so the rotated file's unread tail is still read.
- The thread never blocks the scheduler or the JSONL writer: it sends
  `{"type": "events", "events": [...], "state": {...}}` records to the
  SQLite writer's queue (waiting for room rather than dropping), and each
  record's events and resume point commit in one transaction. A failure is
  listed in `errors` as `events` and retried with backoff up to 5 minutes.
- Retention: `retention.events_days`, default 90.
- `history.db` (with `-wal` and `-shm`) is mode 0600 and the log directory
  0700, narrowed on start if an older daemon left them readable: they name
  other users' programs, units and package commands.
- Without journal access the thread records only what the user's journal
  holds, and says so once in `errors`.

## Record format (schema 3)

Every record has `type` (`meta` or `sample`), `schema`, `t` (Unix seconds) and
`time` (local, with offset). A sample has one key per module that ran on that
tick and `errors` (`{module: message}`) when any failed; readers treat every
module key as optional. A meta record carries the effective config, each
module's interval and static `info` (hardware, topology, units, limits), and
`failed_modules`. Units are in field names (`_bytes`, `_bps`, `_pct`, `_mhz`,
`_c`, `_w`, `_ms`, `_s`, `_ps`). Types live in `crates/schema`; every
collector's output is tested to round-trip through them.

- Devices are keyed by stable id: GPUs by PCI address, dm devices by dm name,
  hwmon chips by `name/device`, filesystems by mount point (one entry per
  device, `mounts` lists the rest).
- `gpu`: `devices.<pci>` with `vendor`, `driver`, `backend` and common optional
  fields; `processes[]` from NVML or DRM fdinfo; `backend_errors.<pci>`.
- `cpu`: usage with guest time split out, per-core use and frequency, `temp_c`
  (package), `temps_c` (CCD/core), `power_w` (RAPL where readable); meta has
  vendor and topology (L3 groups, hybrid core types, preferred-core ranking).
- `processes`: top lists by CPU, memory and I/O, totals by name, and `table`
  (every row, kept out of the crash log); rows carry `start_time`.

## Config

```toml
[general]
interval = 2          # whole seconds
fsync = true

[retention]
log_days = 2
raw_hours = 48
rollup_1m_days = 90
rollup_15m_days = 730
proc_rollup_days = 7
events_days = 90

[modules.<name>]      # every module: enabled, interval; plus its own options
```

Unknown sections, modules or keys, including a module's own options, stop the
daemon at startup with a message naming them.

## Status and next steps

1. ~~Collector: core, modules, SQLite, socket, per-process history.~~ Done.
2. ~~The app (`docs/APP-DESIGN.md`): every screen, Logs and event markers.~~
   Done, released as 0.2.0.
3. Alerts in `sysmand`.

## Not in scope yet

Docker container names in the collector, ARM CPUs, systemd service and
mdraid collectors.
