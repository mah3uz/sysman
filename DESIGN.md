# sysman — design

A Linux system monitor: a collector daemon (`sysmand`) that records metrics, and a
GPUI app (`sysman`) that shows them live and as history. It replaces the Python
`sensor-log` collector in `sensor-log/`, which stays as the reference
implementation until every module is ported (see `sensor-log/OVERVIEW.md`).

## Decisions

| Topic | Decision |
| --- | --- |
| Order | Port the full collector to Rust first, then build the app. |
| History | SQLite, plus a small fsync'd JSONL file kept as crash evidence. |
| Live data | The daemon serves it over a Unix socket. |
| Paths | XDG directories. Existing `sensor-log` data is not migrated. |
| DB model | Numeric series with rollups; list-shaped data as JSON snapshots. |
| Processes | Top N stored every 10 s; the full table served on demand over the socket. |
| JSONL | Today and yesterday, uncompressed. |
| NVIDIA | NVML in-process; after an NVML timeout, `nvidia-smi` with a kill timeout until restart. |
| Hardware | Vendor-neutral records. GPUs: NVIDIA, AMD, Intel (i915 and xe), generic DRM fallback. CPUs: x86 (AMD, Intel). |
| Verification | This machine for AMD CPU, NVIDIA and amdgpu. Fixtures built from kernel docs for the rest; such backends are marked unverified on hardware. beszel (henrygd/beszel) is a reference for data sources. |

## Non-negotiables (carried over from sensor-log)

1. **The last reading before a crash is on disk.** Every JSONL line is written
   and `fdatasync`'d before the next tick.
2. **One failing source does not stop the others.** Every collector runs on its
   own thread; a tick never waits on a collector past its deadline.

## Layout

```
Cargo.toml                 workspace
crates/schema              record types (serde), shared by daemon and app
crates/sysmand             daemon: config, collectors, scheduler, JSONL, SQLite, socket
crates/sysman              GPUI app (after the collector reaches parity)
```

| What | Path |
| --- | --- |
| Config | `$XDG_CONFIG_HOME/sysman/config.toml` (default `~/.config/sysman/config.toml`) |
| History | `$XDG_DATA_HOME/sysman/history.db` (default `~/.local/share/sysman/`) |
| Crash log | `$XDG_DATA_HOME/sysman/log/YYYY-MM-DD.jsonl` |
| Socket | `$XDG_RUNTIME_DIR/sysman.sock` |
| Service | `~/.config/systemd/user/sysmand.service` |

## Daemon

### Collectors

```rust
trait Collector: Send {
    fn meta(&mut self) -> Option<Value> { None }
    fn sample(&mut self) -> anyhow::Result<Option<Value>>;
}
```

Modules are registered in code (no runtime discovery). Each has a constructor
taking its config table and a `Sys` handle. All `/proc` and `/sys` reads go
through `Sys`, whose root defaults to `/` and points at a fixture directory in
tests. A constructor error disables the module and is listed in the meta
record's `failed_modules`, as in sensor-log.

Each collector builds a typed struct from `schema` and returns it as a
`serde_json::Value`, so the daemon handles modules generically and the app reads
the same types back.

### Scheduling and isolation

- Ticks land on multiples of the base interval on the wall clock;
  `tick = round(now / base)`; a module runs when `tick % every == 0`.
- Each module owns a worker thread. The scheduler sends due modules a request
  and collects replies until all arrive or the deadline (`0.75 × base`) passes.
- A module that misses the deadline gets `errors.<name> = "timeout"`. While its
  previous request is still running it is not sent a new one and gets
  `"busy: running since <time>"`. A late reply is discarded.
- At startup every collector is sampled once and the result discarded, to
  prime rate counters.
- A sample is written only if some module produced data or an error.

### GPU isolation

The gpu module runs NVML calls on an inner thread with a 1.5 s deadline. On
the first timeout it abandons that thread, records the switch in `errors`, and
uses `nvidia-smi` from then on. A `nvidia-smi` child that times out is killed
and never waited on (a child stuck in the driver cannot be reaped), and at most
one child is outstanding. NVML is not retried until the daemon restarts.

### Writers

The scheduler hands each record to two writers:

1. **JSONL** (scheduler thread): compact JSON, one line, `write` +
   `sync_data` (`fdatasync`). A meta line at startup and at local midnight.
   Files older than `retention.log_days` are deleted at rollover.
2. **SQLite** (own thread, bounded channel; when full, records are dropped and
   counted, so the database can never delay the crash log). WAL mode,
   `synchronous=NORMAL`, one transaction per record.

### Database

```sql
series    (id INTEGER PRIMARY KEY, path TEXT UNIQUE)
points    (series_id, t, value, PRIMARY KEY (series_id, t)) WITHOUT ROWID
rollup_1m (series_id, t, min, avg, max, count, PRIMARY KEY (series_id, t)) WITHOUT ROWID
rollup_15m(same as rollup_1m)
snapshots (module, t, data, PRIMARY KEY (module, t)) WITHOUT ROWID
errors    (t, module, message)
meta      (t INTEGER PRIMARY KEY, data TEXT)
```

- `t` is integer Unix seconds; the base interval is a whole number of seconds.
- **Series** are the numeric leaves of a sample, named by JSON pointer
  (RFC 6901), for example `/hwmon/k10temp~10000:00:18.3/temp/Tctl`. Leaves under
  an array of objects are not series (a ranked list's index has no identity);
  devices are therefore keyed by stable id in maps, never in arrays. `null` is
  skipped.
- **Snapshots** hold the full module JSON for modules that declare it
  (`processes`, `cgroups`, `netinfo`, `storage`, `power`).
- **Rollups** are accumulated in memory as points arrive and written when a
  bucket closes; 15-minute buckets merge 1-minute ones. A bucket open across a
  restart holds only the post-restart points; `count` shows it.
- **Retention** runs hourly, as per-series range deletes on the primary key.
  `meta` is kept forever.

| Data | Default retention |
| --- | --- |
| `points`, `snapshots`, `errors` | 48 h |
| `rollup_1m` | 90 days |
| `rollup_15m` | 730 days |
| JSONL | 2 days |

Rough size at the current ~330 series per 2 s tick: ~14 M points/day, so the
48 h raw window is a few hundred MB. To be measured once running.

### Socket

Line-delimited JSON over `$XDG_RUNTIME_DIR/sysman.sock`, mode 0600. One request
per line, one response per line:

| Request | Response |
| --- | --- |
| `{"cmd":"meta"}` | latest meta record |
| `{"cmd":"latest"}` | latest sample merged per module (each module's most recent value) |
| `{"cmd":"subscribe"}` | meta, then every sample as it is produced, until the client disconnects |
| `{"cmd":"processes"}` | full process table; CPU % is relative to this client's previous `processes` request |

A slow subscriber's queue is bounded; when full, the subscriber is
disconnected rather than delaying anyone else. History is not served over the
socket: the app opens `history.db` read-only (WAL allows concurrent readers).

## Record format (schema 3)

Same envelope and unit-suffix conventions as sensor-log schema 2
(`sensor-log/OVERVIEW.md` §4): `type`, `schema`, `t`, `time`, one key per
module, `errors`. Changes from schema 2:

- **Devices are keyed by stable id, never listed in arrays**, so they become
  series. GPUs are keyed by PCI address.
- **`gpu` is vendor-neutral.** `gpu.devices.<pci>` has `vendor`, `backend`, and
  common optional fields (utilisation, memory, temperature, power, clocks, fan,
  encode/decode). `gpu.processes[]` comes from NVML for NVIDIA and from
  `/proc/<pid>/fdinfo` (DRM usage stats) for the other drivers.
- **`cpu` gains normalised sensors:** package temperature from whichever
  source exists (`k10temp`, `zenpower`, `coretemp`), and package power where
  RAPL is readable. Its meta gains topology: core types (Intel P/E), L3/CCD
  groups, packages.
- `hwmon` stays a raw dump of every sensor.

The exact per-vendor field mapping is in `docs/research/hardware-sources.md`.

## Config

```toml
[general]
interval = 2          # whole seconds; the shortest interval any module can have
fsync = true

[retention]
log_days = 2
raw_hours = 48
rollup_1m_days = 90
rollup_15m_days = 730

[modules.<name>]      # every module: enabled, interval; plus its own options
```

Unknown sections, modules or keys, including a module's own options, stop the
daemon at startup with a message naming them.

## Port plan

1. Workspace, `schema` envelope, daemon core: config, `Sys`, scheduler and
   isolation, JSONL writer, `--once`. One simple module (`pressure`) end to end.
2. Port the remaining modules, a few at a time, each compared against
   `sensor-log --once` on this machine and tested against fixtures.
3. SQLite writer, rollups and retention.
4. Socket server.
5. Service unit; run alongside sensor-log for a day; compare; switch off
   sensor-log.
6. The app.

## Not in scope yet

Privileged helper (SMART, RAPL where root-only, other users' process I/O),
clean-shutdown marker, Docker container names, ARM CPUs.
