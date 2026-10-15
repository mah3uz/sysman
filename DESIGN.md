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
| Verification | This machine for AMD CPU, NVIDIA and amdgpu. Fixtures built from kernel docs for the rest, marked unverified on hardware. beszel (henrygd/beszel) is a reference for data sources. |
| Sensors | Super I/O chips (nct67xx) read every 10 s, everything else every 2 s. |
| App design | `docs/APP-DESIGN.md`. v1 has every screen, Disk Map included. |
| Menu bar | A DankMaterialShell bar widget reading the socket, after the app's v1. |
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
crates/sysmand             daemon: config, collectors, scheduler, JSONL, SQLite, socket
crates/sysman              GPUI app (not started)
dist/sysmand.service       systemd user unit
docs/research/             data-source and design research
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
| JSONL | 2 days | `log_days` |
| `meta` | forever | |

### Socket

Line-delimited JSON over `$XDG_RUNTIME_DIR/sysman.sock`, mode 0600. One request
per line, one response per line; errors are `{"error": "…"}`.

| Request | Response |
| --- | --- |
| `{"cmd":"meta"}` | latest meta record |
| `{"cmd":"latest"}` | latest sample's envelope plus each module's most recent value (includes `processes.table`) |
| `{"cmd":"subscribe"}` | meta, then every record as produced; a client 32 lines behind is disconnected |
| `{"cmd":"processes"}` | full process table now, CPU relative to this client's previous request (first answer takes ~0.5 s) |

A second daemon is refused when the socket answers; any other socket failure
leaves the daemon logging without it. History is not served over the socket:
the app opens `history.db` read-only (WAL allows concurrent readers).

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

[modules.<name>]      # every module: enabled, interval; plus its own options
```

Unknown sections, modules or keys, including a module's own options, stop the
daemon at startup with a message naming them.

## Status and next steps

1. ~~Collector: core, modules, SQLite, socket, per-process history.~~ Done.
2. The app (`docs/APP-DESIGN.md`): Dashboard, Processes, GPU, Network and Disk
   done; Energy, Hardware, Explorer, Insights, Groups and Disk Map to come.
3. Alerts in `sysmand`; DMS bar widget.

## Not in scope yet

Privileged helper (SMART, RAPL where root-only, other users' process I/O),
clean-shutdown marker, Docker container names, ARM CPUs, systemd service and
mdraid collectors.
