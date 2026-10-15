# Beszel agent collectors vs sysman

Scope: Linux collectors in beszel `agent/` (shallow clone of main) and the gopsutil it calls (`github.com/shirou/gopsutil/v4 v4.26.9`, pinned in beszel `go.mod`). GPUs and CPU temperature/power/topology are out of scope (covered elsewhere).

Path notation:
- `B:` = beszel repo path, e.g. `B:agent/disk.go:630` is line 630 of `agent/disk.go`.
- `G:` = gopsutil v4.26.9 path, e.g. `G:disk/disk_linux.go:503`.
- Paths under `/proc` or `/sys` are what gopsutil actually reads. `HostProc`/`HostSys` honour `HOST_PROC`/`HOST_SYS` env overrides (`G:internal/common/common.go`).

Every claim below was read in the code. Statements marked "(unverified)" come from outside the source.

## 0. Architecture facts that affect the port

- Beszel is a pull model. The hub asks for a snapshot with a `cacheTimeMs` (default 60 000, `B:agent/agent.go:24`).
  - Every rate is kept per `cacheTimeMs` (`lastCpuTimes[cacheTimeMs]`, `diskPrev[cacheTimeMs]`, `netIoStats[cacheTimeMs]`, docker trackers, `B:agent/cpu.go:11-12`, `B:agent/disk.go:692`).
  - A fresh response is reused for `cacheTimeMs/2` (`B:agent/agent_cache.go:39`).
  - We sample on a timer and keep one baseline per metric, so we do not need this. The lesson is only "never let two readers share one baseline".
- The agent runs as an ordinary user. Everything is read from /proc, /sys, statfs, the D-Bus system bus and a Docker/Podman socket. Only SMART and ZFS CLIs need extra rights.
- Env vars are read as `BESZEL_AGENT_<KEY>` first, then `<KEY>` (`B:agent/utils/utils.go:17-22`). Config knobs mentioned below (`FILESYSTEM`, `EXTRA_FILESYSTEMS`, `NICS`, `SENSORS`, ...) all go through this.
- Beszel stores bytes as GB/MB floats rounded to 2 decimals (`B:agent/utils/utils.go:24-30`). We should keep raw integers.

## 1. CPU usage %

Source: gopsutil `cpu.Times()` reads `/proc/stat` (`G:cpu/cpu_linux.go:106`, parser `G:cpu/cpu_linux.go:351-385`).
- Fields: user nice system idle iowait irq softirq steal guest guest_nice.
- Steal/guest columns are optional (older kernels).
- Percent per core uses the cpuN lines only.

Derivation (`B:agent/cpu.go:121-148`):
- `total = user+system+idle+nice+iowait+irq+softirq+steal+guest+guest_nice`, then subtract guest and guest_nice. They are already inside user/nice, so counting them twice is the classic bug.
- `busy = total - idle - iowait`. iowait counts as idle.
- `pct = (busy2-busy1)/(total2-total1)*100`, clamped to 0..100.
- Breakdown (`B:agent/cpu.go:75-82`): `user, system, iowait, steal, idle` each as `delta/totalDelta*100`, clamped. nice/irq/softirq are only visible inside the total.

Edge cases:
- `totalDelta <= 0` returns all zeros (`B:agent/cpu.go:71-73`).
- The baseline is replaced before validating (`B:agent/cpu.go:65`), so a counter wrap or reset re-baselines instead of poisoning the next sample.
- Per-core usage is `uint8(round(pct))` (`B:agent/cpu.go:114`). It is truncated to the min of old and new core count, so hotplug does not panic (`B:agent/cpu.go:108`).
- Init seeds a baseline at process start (`B:agent/cpu.go:16-23`), so the first report is a real delta.
- Container/LXC mode (`B:agent/cpu_linux.go:18-120`):
  - Inside LXC, lxcfs serves `/proc/stat` with host-core counters.
  - Beszel detects lxcfs by finding `fuse.lxcfs` mounted on `/proc/stat` in `/proc/self/mountinfo` (`:50-88`). It also checks env `container=lxc` and `/run/systemd/container`.
  - In that case it uses cgroup `cpu.stat` `usage_usec` (v2) or `cpuacct.usage` (v1) divided by `elapsed * usable_cores`.
  - `usable_cores` = min(`runtime.NumCPU`, cpuset, `cpu.max` quota) (`:110-118`).
  - Per-core usage is skipped in this mode (`B:agent/system.go:178-185`).
- Static info: model from `/proc/cpuinfo`, with a fallback for MIPS (`B:agent/system.go:84-93, 274-315`).
  - Logical count is `processor` lines in `/proc/cpuinfo`, falling back to `^cpuN` lines in `/proc/stat` (`G:cpu/cpu_linux.go:390-415`).
  - Physical count is unique `topology/core_cpus_list` or `thread_siblings_list` under `/sys/devices/system/cpu` (`G:cpu/cpu_linux.go:424-432`).

Privilege: none.

## 2. Load average, uptime, boot

- Load: `load.Avg()` reads `/proc/loadavg` (`G:load/load_linux.go:120`; called at `B:agent/system.go:188`). Only 1/5/15 are used. Beszel does not read the running/total or last-pid fields.
- Uptime: reads `/proc/uptime` first field directly (`B:agent/uptime_linux.go:15-44`).
  - It deliberately avoids `host.Uptime()` (sysinfo(2)) because lxcfs virtualises the file but not the syscall.
  - It falls back to gopsutil only on read or parse failure.
- Not collected by beszel: ctxt, intr, forks, procs_running/blocked, btime, `/proc/sys/fs/file-nr`.

## 3. Memory

Source: `mem.VirtualMemory()` reads `/proc/meminfo` only (`G:mem/mem_linux.go:34`).
- Keys include MemTotal, MemFree, MemAvailable, Buffers, Cached, SReclaimable, Shmem, SwapTotal, SwapFree.
- `Cached += SReclaimable`, same as `free` and psutil (`G:mem/mem_linux.go:312-314`).
- If MemAvailable is missing (pre-3.14), `Available` is estimated from `/proc/zoneinfo` or `Cached+Free` (`:316-321, 409`).
- `Used = Total - Available` (`G:mem/mem_linux.go:323`).

What beszel counts as "used" (`B:agent/memory.go:18-55`, `B:agent/system.go:329-347`):

| Quantity | Default | `MEM_CALC=htop` |
|---|---|---|
| used | `Total - Available` (if gopsutil's Used > Total, recomputed with saturating sub) | `Total - Free - buff/cache` |
| buff/cache | `min(Cached,Total) + min(Buffers, ...) - min(Shmem,Total)` (Shmem is not cache) | same |
| fallback | if Cached and Buffers are both 0: `Total - Free - used` | same |

- All subtractions are saturating, because `/proc/meminfo` can change mid-read (`B:agent/system.go:327-358`).
- ZFS ARC (`B:agent/memory.go:36-40`, `B:agent/zfs/zfs_linux.go:26-49`):
  - It reads `size` from `/proc/spl/kstat/zfs/arcstats`.
  - If `0 < arc < used`, it subtracts ARC from used and reports it as `MemZfsArc`.
  - Rationale: ARC is reclaimable but the kernel does not put it in MemAvailable.
  - Detection runs once at start (`B:agent/system.go:121`).
- Swap (`B:agent/memory.go:24-25`): `SwapTotal - SwapFree` from meminfo. SwapCached is not subtracted, matching `free`.
- Container mode (only with `USE_CGROUP=true`; LXC keeps meminfo because lxcfs already virtualises it, `B:agent/memory_linux.go:12-82`):
  - Reads the cgroup mount root, not the agent's own leaf.
  - v2: `memory.current`, `memory.max`, `memory.stat` (`file`, `shmem`). v1: `memory.usage_in_bytes`, `limit_in_bytes`, `total_cache`, `total_shmem`.
  - `used = usage - (file_cache - shmem)`.
  - v1 "unlimited" is treated as 0 when the limit is `>= 2^63 - 1MiB` or equals 2147479552.
  - Total = min(limit, host MemTotal).
- Not read by beszel: `/proc/vmstat`, zram `mm_stat`, SwapCached, Dirty, Slab, HugePages.

## 4. Disk I/O

### 4.1 Which devices are tracked

Beszel does not report every block device. It reports the root filesystem's device plus a small explicit set, then looks up each in `/proc/diskstats` (`B:agent/disk.go:337-412`).
- Root device: `FILESYSTEM` env, else the partition mounted at the root mount point, else the busiest device (`mostActiveIoDevice`, `B:agent/disk.go:253-265, 511-528`).
- Extra devices: `EXTRA_FILESYSTEMS` entries (`device__customName`, matched by mountpoint or device-name suffix, `B:agent/disk.go:267-301`).
- Extra devices also come from direct children of `/extra-filesystems` (the Docker bind-mount convention). Nested mounts are ignored so a bind-mounted host root does not drag in /proc, /sys (`B:agent/disk.go:303-314`).
- Root mount point is `/`, or `/sysroot` on ostree/Silverblue/CoreOS/Flatcar images (`B:agent/disk.go:829-863`).

### 4.2 Mapping mount to block device

Chain (`B:agent/disk.go:106-176`):
1. Mount source from the mountinfo parser (see 5.1). `/dev/mapper/*` is resolved by `EvalSymlinks`, and `/dev/root` is resolved via `/sys/dev/block/<maj:min>` (`G:disk/disk_linux.go:421-442`).
2. `key = basename(device)`. If the device is an absolute symlink, also compute `resolvedKey = basename(EvalSymlinks(device))`. This handles `/dev/vg/lv` to `dm-3` (`B:agent/disk.go:116-123`).
3. `findIoDevice` (`B:agent/disk.go:472-509`):
   - Exact match on diskstats name, or on the label.
   - Otherwise any name that is a prefix of the other, with the highest total bytes winning (ties by ops, then name). This is how `nvme0n1p2` maps to `nvme0n1`, `sda1` to `sda`, and `ada0p2` to `ada0`.
4. Label for dm devices is `/sys/block/<dev>/dm/name` (so `vg-lv` matches), else udev `E:ID_FS_LABEL` from `/run/udev/data/b<maj>:<min>` (`G:disk/disk_linux.go:644-680`).
5. Non-root mounts that cannot be matched keep the raw key. At init a missing diskstats entry only logs "Device not found in diskstats" and the device is dropped from I/O (`B:agent/disk.go:614-619`).

### 4.3 Counters and rates

Source: `/proc/diskstats` (`G:disk/disk_linux.go:503-600`).
- Lines with fewer than 14 fields are skipped. All-zero devices are dropped (`:586`).
- Sector size is a constant 512 (`:24`, applied at `:574-575`).
- Filtering of partitions is not done by gopsutil. All of sda, sda1, dm-0, loop0, md0, nvme0n1 and nvme0n1p1 are returned. Beszel narrows by name (4.1).
- NVMe: `getDeviceName` rewrites `nvme0c0n1` to `nvme0n1`, but only for the serial/label lookups (`G:disk/disk_linux.go:682-698`).

Rates (`B:agent/disk.go:688-803`):
- MB/s: `(bytes - prevBytes) * 1000 / msElapsed`.
- Derived metrics:
  - read-time % and write-time %: `Δread_ms / ms_elapsed * 100`.
  - io-util %: `Δio_ticks_ms / ms_elapsed * 100`.
  - weighted-io (queue depth * 100): `Δweighted_ms / ms_elapsed * 100`.
  - r_await and w_await: `Δread_ms / Δreads` and `Δwrite_ms / Δwrites`, 0 if no ops.
- Guards:
  - Skip when `msElapsed < 100` (`:719`).
  - The first sample of an interval must span at least half the interval, so startup bursts are not recorded as a rate (`:725-727`).
  - Reject a sample as invalid if either rate exceeds 50 000 MB/s, then re-baseline everything (`:735-740`).
  - 32-bit wrap handling: `/proc/diskstats` prints the ms fields as 32-bit unsigned, so `ioTimeDelta` adds `2^32` on wrap (Linux only, `:819-827`). The byte and ops counters are not wrap-protected.

Not collected: IOPS (read_count/write_count deltas), merged ops, discard/flush columns (fields 15+), in-flight count. Beszel only uses r/w await, util and weighted time.

Privilege: none.

## 5. Filesystem usage

### 5.1 Enumeration

`disk.PartitionsWithContext(ctx, all=true)` (`B:agent/disk.go:343`).
- Reads `/proc/1/mountinfo`, falling back to `/proc/self/mountinfo` (`G:disk/disk_linux.go:263-280`), then to `…/mounts` on very old kernels.
- `/proc/1/mountinfo` is typically unreadable without root, so non-root runs fall back to `self`. In a container, `self` is the container's mount namespace.
- With `all=true` there is no fstype filter. With `all=false` gopsutil keeps only fstypes in `/proc/filesystems` that are not `nodev`, plus zfs (`G:disk/disk_linux.go:376-380, 480`). Beszel uses `all=true` and then selects.
- The mountinfo parser (`G:disk/disk_linux.go:302-440`) gives us several useful tricks:
  - Splits on ` - `, so optional fields are handled.
  - Takes the "device" as the mount source if it starts with `/`; otherwise uses the root-dir field for virtual filesystems.
  - Dedupes by block-device id (`maj:min`, field 3): later mounts of the same `maj:min` reuse the first device path (`seenDevIDs`, `:339-406`).
  - Bind detection: `rootDir != "/"` and the source starts with `/`, unless the btrfs `subvol=` option equals `rootDir` (then it is a subvolume mount, not a bind) (`:408-424`). With `all=false` binds are skipped.
  - Effective ro/rw is the combination of mount options and superblock options (`:462-485`).
  - Octal-unescapes mountpoints (`\040`) (`unescapeFstab`).

### 5.2 What beszel includes or excludes

Only these are reported (`B:agent/disk.go:378-402`):
1. Root filesystem (marked `Root`, and its values drive the headline `DiskTotal/Used/Pct`).
2. `EXTRA_FILESYSTEMS`.
3. Direct children of `/extra-filesystems`.

Everything else is silently ignored: /boot, /home on other devices, tmpfs, NFS, and so on. De-duplication:
- By I/O key: a filesystem whose resolved key already exists is skipped (`B:agent/disk.go:167-169`). So two subvolumes or two bind mounts of one device collapse.
- By usage equality: any extra filesystem whose total and used are both within 16 MiB of root's is dropped (`pruneDuplicateRootExtraFilesystems`, `B:agent/disk.go:414-462`). This catches bind-mounts of root and btrfs subvolumes seen as extra.
- Docker-injected `/etc/hosts`, `/etc/resolv.conf`, `/etc/hostname` mounts whose device starts `/dev` are accepted as root evidence inside containers (`B:agent/disk.go:92-98, 221-224`).
- Btrfs and ZFS pools get their own entries and are chart-deduped against the matching filesystem by `BTRFS_IOC_FS_INFO` fsid (`B:agent/storage_pool.go:486-514`, `B:agent/btrfs/btrfs_linux.go:355-365`).

### 5.3 Values

- `statfs(2)` via `disk.Usage` (`G:disk/disk_unix.go:13-54`):
  - `Total = Blocks*Bsize`.
  - `Used = (Blocks-Bfree)*Bsize`.
  - `Free = Bavail*Bsize`.
  - `UsedPercent = Used/(Used+Free)*100` (matches `df`, excludes the root-reserved blocks from the denominator).
  - Inodes are computed but not used by beszel.
- ZFS datasets: use `zfs list -Hp` used/avail instead, because statfs on a dataset omits child datasets (`B:agent/disk.go:638-658`, `B:agent/zfs/zfs.go:145`).
- On statfs error the entry's totals are zeroed and it is skipped (`B:agent/disk.go:659-671`).
- `DISK_USAGE_CACHE=15m` skips non-root statfs calls between refreshes, to avoid waking spun-down disks (`B:agent/disk.go:630-636`).

Privilege: none (statfs on a mountpoint needs search permission only).

## 6. Network

Source: `psutilNet.IOCounters(true)` reads `/proc/net/dev` (`G:net/net_linux.go:43`). Beszel never reads `/sys/class/net/*/statistics`. Content is equivalent, but only the 8 standard columns are used (bytes, packets, errs, drop per direction).

Interface selection (`B:agent/network.go:235-260`):
- Auto-exclude if the name has prefix `lo`, `docker`, `br-`, `veth`, `bond`, `cali`, or if either rx or tx bytes is exactly 0 at selection time.
- `NICS` env: a whitelist, or a blacklist if it starts with `-`. Supports `*` via `path.Match`.
  - Whitelist mode skips the auto filter, so the user can include a `veth` or `lo`.
  - Blacklist mode still applies it (`B:agent/network.go:16-77, 240-245`).
- The interface set is frozen at start (`B:agent/network.go:92-123`). It is rebuilt only if empty (`:126-134`, issue #466 comment) or after the invalid-rate reset (`:223`). Hot-plugged interfaces are missed. We should not copy this.
- Prefix matching (`lo*`, `bond*`) is crude. `lo` also matches e.g. `lowpan0`. Excluding `bond*` avoids double-counting slaves, but it also hides bond traffic when the slaves are not otherwise visible.

Rates and resets (`B:agent/network.go:159-205`):
- A rate is computed per NIC, then summed (not diff-of-sums). This keeps the total from underflowing when an interface disappears.
- Counter decrease (reset or wrap): `delta = current` (treat as having restarted from 0) (`:182-186, 189-193`).
- Whole-sample sanity: if sent or received exceeds 10 GB/s, log, re-init, and report 0 (`:215-228`).
- First sample (no previous tracker value or `msElapsed == 0`) yields 0.
- NVIDIA Jetson special case: for the `nvethernet` driver (`/sys/class/net/<if>/device/driver` symlink), use ethtool `mmc_tx_octetcount_gb[_h]` and `mmc_rx_octetcount_gb[_h]` instead of the inflated kernel counters (`B:agent/network_counters_linux.go:16-71`). The SIOCETHTOOL ioctl needs no root.

Wi-Fi (`B:agent/wifi/wifi_linux.go:85-111`, `B:agent/wifi/README.md`):
- nl80211 over generic netlink (`mdlayher/wifi`): SSID and RSSI per connected station interface only. No scanning, no bitrate, no link quality.
- Gated on `/sys/class/ieee80211` being non-empty. Otherwise the nl80211 family lookup would make the kernel modprobe cfg80211.
- Default interval only. `SKIP_WIFI=true` disables.
- No root needed. Our `/proc/net/wireless` gives link quality and noise but not SSID.

Not collected: per-NIC packets/errors/drops, `/proc/net/snmp`, sockstat, IP addresses/routes/DNS, link speed/state.

## 7. Temperature sensors (generic hwmon path only)

Source: `sensors.TemperaturesWithContext` (`G:sensors/sensors_linux.go:20-110`).
- Glob `/sys/class/hwmon/hwmon*/temp*_input`. If it finds nothing, glob `…/hwmon*/device/temp*_input` (CentOS layout). If still nothing, read `/sys/class/thermal/thermal_zone*/{type,temp}`.
  - The fallbacks are all-or-nothing: they trigger only when the previous glob matched zero files. A mixed system gets hwmon only.
- Name: `<chip name>[_<label>]`. `<chip name>` is the `name` file. The label is `temp<N>_label`, lowercased with spaces turned to `_` (`"Core 0"` becomes `coretemp_core_0`).
  - A chip without a `name` file is skipped entirely (the warning is dropped).
- Value is `/1000`. `_max` and `_crit` are also read but beszel discards them.

Beszel post-processing (`B:agent/sensors.go:115-183`):
- Values `0 < t < 1` are rescaled (some drivers report fractions). It tries `*100` if the result is in 15..95, else `*1000` if in 15..95, else `*100` (`:255-269`).
- Drops `t <= 0` or `t >= 200`.
- Duplicate name: append `_<index in list>` (`:162-166`).
- `SENSORS` env: whitelist, or blacklist with a `-` prefix, with `*` globs. `SENSORS=""` disables collection. `PRIMARY_SENSOR` picks the dashboard value, default max across all (`:175-180`). `SYS_SENSORS` overrides the sysfs root.
- 2 s timeout (10 s on first run) around the read, with panic recovery and one retry (`:56, 197-223`).
- `SKIP_GPU`: builds a temp symlink shadow of `/sys/class/{hwmon,thermal}` without GPU chips, because gopsutil reads every `temp*_input` before filtering and reading wakes an idle dGPU (`:307-368`).
- Fans (`B:agent/fans.go:71-121`):
  - Globs `fan*_input` per hwmon chip, with a `device/` fallback (applesmc).
  - Key is `<chip>_<fanN_label or fanN>`. Zero RPM is kept. The discovery list is cached for the process lifetime.
- Not read: `in*_input` (voltage), `power*`, `curr*`, `humidity*`, `*_alarm`, `*_crit`.

## 8. Battery

`B:agent/battery/battery_linux.go:39-85`. For each entry in `/sys/class/power_supply`:
- Require `type == "Battery"`, then readable integer `capacity`, clamped to 0..100.
- Name: `model_name`, else `model`, else the directory name. Then it is UTF-8-sanitised and de-duplicated with `" (2)"` (`B:agent/battery/battery.go:33-50`).
- State from `status`: Empty, Full, Charging, Discharging, "Not charging" (idle), else unknown.
- `System = (scope != "Device")`. Peripherals such as HID mice and headsets report `scope=Device`.
- `charge_full` or `energy_full` is used to rank the "primary" battery: has-capacity, then larger capacity, then system scope, then name (`B:agent/battery/battery.go:54-73`).
- Not read: `energy_now`, `power_now`, `voltage_now`, `cycle_count`, `capacity_level`, `technology`, `health`, `time_to_empty`, AC `online`.

## 9. systemd services

`B:agent/systemd.go`, Linux only, via D-Bus (`github.com/coreos/go-systemd/v22/dbus`, system bus).
- Gate: `/run/systemd/system`, the dbus socket, or `/proc/1/comm == systemd`; then verify the `Version` manager property on the bus (`:82-122`). `SKIP_SYSTEMD=true` disables.
- Listing: `ListUnitsByPatterns(["loaded"], patterns)`, default pattern `*.service`. `SERVICE_PATTERNS` adds globs (names without `.service` or `timer` suffix get `.service`) (`:198, 417-435`).
- Skips units that never became active: `ActiveEnterTimestamp` of 0 or MaxUint64 (`:237-243`).
- Per unit: `ActiveState`, `SubState` from the list, plus `Service.MemoryCurrent`, `Service.MemoryPeak`, `Service.CPUUsageNSec`. `MaxUint64` means unavailable. Peak is bumped if current exceeds it (`:251-286`).
- CPU % = `ΔCPUUsageNSec / (Δwall_ns * NumCPU) * 100`, reset to 0 if the counter went backwards (`B:internal/entities/systemd/systemd.go:81-112`).
- Refresh: primed at start, then every 10 minutes (`:143-148`). The failed-service count is derived from the cache (`:157-167`).
- Details on demand: `GetUnitProperties` plus `MainPID`, `TasksCurrent`, `NRestarts`, `Result`, etc. (`:299-345`).
- Logs: `journalctl` subprocess. Probed once with `journalctl --system --lines 1`; works only if the user is in `systemd-journal`/`adm` (`:30-47`).
- Privilege: read-only D-Bus property access works for any user by default polkit/dbus policy. Journal needs group membership.

## 10. Docker / Podman containers

`B:agent/docker.go`. All data comes from the engine's HTTP API over a socket. The agent never reads cgroups for containers, so no root is needed beyond socket access.

Socket and detection:
- `DOCKER_HOST` env (`unix://`, `tcp://`, `http(s)://`), else the first existing of `/var/run/docker.sock` and `/run/user/<uid>/podman/podman.sock` (rootless Podman), else it defaults to the Docker path (`:653-700, 811-820`).
- `DOCKER_HOST=""` disables.
- The agent user needs read/write on the socket (docker group, or the user's own Podman socket).
- Podman is detected from the `Server` response header (`:151-155`); Windows likewise (`:156`).
- HTTP timeout 2.1 s (`DOCKER_TIMEOUT`), with a fixed `User-Agent: Docker-Client/` (Docker-for-mac bug) (`:40, 687-702`).

Calls (`:140-236, 506-630`):
- `GET /containers/json` lists running containers only (no `all=1`). Short id = first 12 chars.
- `GET /containers/<id>/stats?stream=0&one-shot=1` per container, 5 concurrent (`sem`, `:726`). One-shot needs Docker >= 25; older versions are queued differently (`:747-769`). Failures are retried once, in parallel.
- Podman health: `GET /containers/<id>/json` (`:476-503`).
- Containers whose status text contains "second" are treated as just restarted, and their baselines are dropped (`:186-189`).
- `EXCLUDE_CONTAINERS`: `path.Match` globs on the name (`:126-137`).

Derivations:
- CPU (`B:internal/entities/container/container.go:54-99`):
  - Docker: `Δcontainer_total_usage / Δsystem_cpu_usage * 100`. This is a fraction of the whole host (100% means all cores), not of one core.
  - Zero when `prev == 0` (first sample) or either counter went down.
  - Podman reports `system_cpu_usage` from cgroup stats, not `/proc/stat`, so beszel uses `Δusage / (Δwall_ns * online_cpus)` instead (`:575-589`).
  - Reject (error) if the result exceeds 100 (`:371-376, 603`).
- Memory (`:268-284`): `usage - inactive_file` (cgroup v2), else `usage - cache` (v1). Error if the result is `<= 0` or `> 100 TB` ("bad memory stats").
- Network (`:313-368`):
  - Sum `rx_bytes`/`tx_bytes` across all of the container's interfaces from the stats response.
  - Delta against the previous sum. A decrease gives 0 for that cycle. Rate above 5 GB/s is discarded.
  - Summed counters cannot distinguish a restart from a removed interface, so the safe answer is 0.
- Also collected: status text and health (parsed from the status string or the API 1.52+ health field), published ports, image, and an image-update check against the registry (`docker_registry.go`, `docker_image_updates.go`, background, outbound HTTPS, `DOCKER_IMAGE_CHECK=false` disables).
- Container logs and inspect JSON on request (`:844-`), with container id validated against `^[a-fA-F0-9]{12,64}$` (`:36, 822-827`).
- Host info (`/info`) feeds OS name, kernel, NCPU and MemTotal when the agent is containerised (`B:agent/system.go:32-42, 78-99`).

## 11. SMART and storage health

Primary: the external `smartctl` binary (smartmontools); no ioctls in Go.
- Discovery: `smartctl --scan -j`, 10 s timeout, re-scanned every 30 min (`B:agent/smart.go:154-190`). `SMART_DEVICES` (with `:type` hints) adds or overrides. `EXCLUDE_SMART` removes devices.
- Collection: `smartctl [-d <type>] -a --json=c [-l devstat (sat/ata)] [-n standby] <dev>`, 15 s timeout (`:520-560, 601-640`).
  - `-n standby` is used only once cached data exists, so sleeping disks are not woken.
  - Exit status 2 (standby) with cached data: keep the cache. Without cached data: rerun without `-n standby`.
  - NVMe: if the controller node (`/dev/nvme0`) fails, retry `/dev/nvme0n1` and remember it (`:562-585`).
- Interval: `SMART_INTERVAL` env, hub-driven (`B:agent/agent.go:124-131`).
- Privilege: `/dev/sdX` and `/dev/nvmeX` ioctls need root or `CAP_SYS_RAWIO` (plus `CAP_SYS_ADMIN` for NVMe) (unverified here; this comes from smartmontools/beszel docs, not from the Go code, which just runs the binary). Beszel is therefore not usable unprivileged for this feature.
- Sysfs-only health sources that need no smartctl and no root:
  - mdraid (`B:agent/mdraid_linux.go:34-170`):
    - Discovers `/sys/block/md*` that have `md/array_state`.
    - Reads `md/{array_state, level, raid_disks, degraded, sync_action, sync_completed, sync_speed, mismatch_cnt}`, plus a faulty-member count from `md/dev-*/state`.
    - Maps these to PASSED, WARNING (resync/recover/reshape, mismatch) or FAILED (inactive, faulty, degraded).
  - eMMC (`B:agent/emmc_linux.go:28-150`): `/sys/class/block/mmcblk*/device/{pre_eol_info, life_time, device_life_time_est_typ_a/b, name, serial, prv}` (hex bytes, 0x01..0x0b wear buckets).
  - btrfs (`B:agent/btrfs/btrfs_linux.go:54-137`):
    - Reads `/sys/fs/btrfs/<uuid>/{label, allocation/{data,metadata,system}/disk_used, devices/*, devinfo/<id>/{missing, replace_target, error_stats}}`.
    - `error_stats` supplies read/write/corruption errs.
    - Missing device means DEGRADED.
    - It falls back to `BTRFS_IOC_FS_INFO`/`DEV_INFO` ioctls on the mountpoint for pre-5.13 kernels.
    - Usage is statfs-based so it matches `df`.
- ZFS (`B:agent/zfs/zfs.go:73-148`, `B:agent/zfs/zfs_linux.go:26-130`):
  - CLI: `zpool list -Hp -o name,size,alloc,free,health` and `zfs list -Hp ...`. Each needs `/dev/zfs` (checked first, so containers fail fast).
  - Kernel stats: `/proc/spl/kstat/zfs/arcstats`, `<pool>/iostats`, `<pool>/objset-*` (world-readable).

## 12. Other things beszel collects

| Item | Source | Notes |
|---|---|---|
| Package updates | `apt`, `dnf`, `zypper`, `checkupdates` (pacman), `apk`, `xbps-install` subprocess (`B:agent/package_updates.go:147-166`) | Hourly default (`PACKAGE_UPDATES_INTERVAL`); skipped in containers; 5 min timeout; pacman/xbps sync at most every 12 h. |
| Network monitors | Hub-pushed ICMP/TCP/HTTP/DNS probes plus TLS cert check (`B:agent/network_monitor_probe.go:24-36`) | Latency monitoring, not host metrics. ICMP may need `CAP_NET_RAW` or ping_group_range (unverified). |
| OS / kernel / hostname | `/etc/os-release` `PRETTY_NAME`, `host.KernelVersion`, `host.KernelArch`, `os.Hostname` (`B:agent/system.go:44-82, 360-379`) | Static, sent as "details". |
| Host ID fingerprint | `host.HostID` / product_uuid with a known-bad UUID denylist (`B:agent/fingerprint.go:12-30`) | Agent identity. |
| Cgroup version/root detection | `/proc/self/cgroup`, `/proc/self/mountinfo`, `cgroup.controllers`, `cgroup.type` (`B:agent/cpu_linux.go:122-140`) | Used for container-scoped CPU/mem. |
| Health file | touches `/dev/shm/beszel_health`, unhealthy if older than 91 s (`B:agent/health/health.go:15-55`) | Docker HEALTHCHECK. |
| Process-level data | none | Beszel has no per-process collector. |
| PSI, `/proc/vmstat`, `/proc/net/snmp`, sockstat, zram, file-nr, ctxt/intr | none | A grep of `agent/` for these found no reads. |

## 13. Comparison and what to adopt

Legend: B better = beszel's method is clearly better; Ours more = we collect something beszel does not; Same = equivalent.

| Area | Our approach | Beszel's approach | Adopt | Why |
|---|---|---|---|---|
| CPU % | `/proc/stat` per-core ticks, differenced | Same file; busy excludes idle+iowait; guest/guest_nice subtracted from total; user/system/iowait/steal/idle breakdown; clamp; re-baseline on wrap | Subtract guest and guest_nice from the total; export `iowait` and `steal` as separate percentages; always replace the baseline before validating the delta; clamp 0..100 | Avoids double counting on VM hosts. Per-core uses `min(len)` so hotplug does not crash (B better on guest handling; Ours more: we also have ctxt/intr/procs). |
| CPU in LXC | Not handled | lxcfs detection, switch to cgroup `usage_usec / (elapsed * cores)` (`B:agent/cpu_linux.go:50-118`) | Optional: detect `fuse.lxcfs` on `/proc/stat` in mountinfo and switch to cgroup accounting | Idle LXC guests otherwise show host-core load. Low priority unless we run in LXC. |
| Load / uptime | `/proc/loadavg`, `/proc/uptime` | Same files (uptime read directly rather than sysinfo) | Nothing | Same. Ours more: ctxt, intr, forks, procs_running/blocked, btime, file-nr. |
| Memory used | meminfo + vmstat + zram | `Total - Available`; cache = `Cached + SReclaimable + Buffers - Shmem`; optional htop formula; swap = `SwapTotal - SwapFree` | Report `used = MemTotal - MemAvailable`, `cache = Cached + SReclaimable + Buffers - Shmem` (Shmem is not reclaimable), saturating subtraction | Matches `free`/htop semantics and survives meminfo changing mid-read. Ours more: vmstat faults/swap/oom, zram. |
| ZFS ARC | Not handled | Subtract ARC (`arcstats` `size`) from used when `0 < arc < used` | Read `/proc/spl/kstat/zfs/arcstats` `size`, report it separately, subtract from used | On ZFS hosts, ARC shows as "used" because it is not in MemAvailable (B better; ARC is real RAM so keep the separate field). |
| Disk I/O device set | All of `/proc/diskstats` | Only root + `EXTRA_FILESYSTEMS`, mapped by basename / symlink / prefix match | Keep ours (all devices), but classify each name: a whole disk exists as `/sys/block/<name>`, a partition does not (it sits under its parent). Optionally map each mount to a disk for display with the `findIoDevice` rules | Beszel misses most disks by design (Ours more). Its mount-to-device mapping (EvalSymlinks for dm, prefix for partitions) is worth copying for labelling. The `/sys/block` rule is our suggestion, not beszel's. |
| Disk rates | bps, iops, await, busy% | MB/s, util %, r_await/w_await, weighted-io, per-read/write time %; 32-bit wrap handling for ms fields; reject >50 GB/s | Add: 32-bit wrap fix on the ms fields (`ioTimeDelta`), a "first sample must cover ≥ ½ interval" rule, sanity cap (e.g. 50 GB/s), skip when `dt < 100 ms`, separate r_await/w_await, queue depth from field 11 | Prevents spikes after start and negative rates. Our iops (count deltas) is something beszel lacks (Ours more). |
| dm / md / nvme | Plain names | Label via `/sys/block/<dev>/dm/name`; nvme controller names `nvmeXcYnZ` rewritten to `nvmeXnZ` for serial lookups; `/dev/root` resolved via `/sys/dev/block` | Read `/sys/block/dm-N/dm/name` for LVM/LUKS display names; list `slaves/` for the underlying devices | Human names for dm-N. Cheap, unprivileged. |
| FS usage | statfs for every mount in `/proc/self/mounts` | statfs for root + extras only; `used = (blocks - bfree) * bsize`, pct = `used/(used+avail)` | Use the same formulas (`df`-compatible pct). Parse `/proc/self/mountinfo` (not `mounts`) and de-duplicate with beszel's rules: `maj:min` seen before, bind = `root != "/"` and source starts with `/`, btrfs subvol is not a bind. Keep unescaping `\040` | We report more mounts (Ours more); the dedupe prevents repeats from btrfs subvols, Docker binds and `/etc/hosts`-style mounts. |
| FS dedupe | none | Drop extras whose total and used are within 16 MiB of root | Prefer `maj:min` / btrfs fsid dedupe, with the size-equality heuristic only as a fallback | Exact IDs are more reliable than size matching (this heuristic can false-positive on two equal empty volumes). |
| Spinning disks | statfs every sample | `DISK_USAGE_CACHE` skips non-root statfs for N minutes | Cache statfs on rotational (`/sys/block/*/queue/rotational=1`) or network mounts at a longer interval (our suggestion) | statfs on a sleeping disk can wake it; on a hung NFS mount it can block (use a worker timeout). |
| ZFS FS usage | statfs (wrong for datasets) | `zfs list -Hp` used+avail | If ZFS is present, use `zfs list` or fall back to statfs with a note | statfs on a dataset omits children. Needs `/dev/zfs` access. Optional. |
| Network interfaces | `/sys/class/net/*/statistics` for all NICs | `/proc/net/dev`; skip `lo, docker, br-, veth, bond, cali` prefixes and zero-byte NICs; NICS whitelist/blacklist | Keep all NICs, but tag virtual ones so a total can exclude them (skip names whose `/sys/class/net/<if>` target lies under `devices/virtual`; our suggestion). Re-enumerate every tick | Beszel's frozen interface list misses hot-plugged NICs and its prefix list is brittle (B worse here). `/proc/net/dev` and sysfs counters are equivalent. |
| Net counter wrap/reset | Differencing | Per-NIC delta; on decrease use `current`; sum per-NIC deltas, not a diff of sums; >10 GB/s sample rejected | Do per-NIC delta; on decrease emit 0 or `current` (a deliberate choice, document it); sum after; drop impossible rates | Totals do not go negative when an interface disappears. Treating a decrease as `current` can overcount after a wrap; 0 is the safer alternative. |
| Wi-Fi | `/proc/net/wireless` | nl80211: SSID + RSSI, gated on `/sys/class/ieee80211` | Optional: nl80211 `GET_STATION` for SSID/bitrate/signal; keep `/proc/net/wireless` as fallback | SSID and bitrate are not in `/proc/net/wireless`. Netlink needs no root. Low priority. |
| Net protocol stats, IP/route/DNS | snmp, sockstat, `ip -j`, resolv.conf | not collected | Keep | Ours more. |
| Temperatures | Every `hwmon*/*_input`, keyed name/device | `hwmon*/temp*_input` only; name = chip + lowercased label; fallbacks `device/`, `thermal_zone*`; drops `t<=0` or `>=200`; fraction rescale; dup suffix; 2 s timeout; whitelist/blacklist globs | Add: sanity window (0..200 °C), thermal_zone fallback when no hwmon, dup-key disambiguation, a read timeout (sysfs reads on some drivers block) | Beszel drops garbage values and handles Raspberry Pi style hosts. Ours more: voltage/power/current/fan/humidity inputs and `_max/_crit` thresholds (beszel reads and discards them). |
| Fans | hwmon inputs | `fan*_input`, `device/` fallback, key `chip_label`, zero RPM kept, cached discovery | Keep ours; add the `device/` fallback; keep 0 RPM | applesmc-style drivers. Cached discovery misses hotplug, so we should not cache. |
| Battery | `/sys/class/power_supply/*` | Only `type=Battery`; `scope=Device` marks peripherals; clamp 0..100; status mapping; primary choice by `charge_full/energy_full` | Keep reading all supplies, but add the `scope` flag and a "primary battery" rule; sanitise firmware strings to valid UTF-8; clamp capacity | Peripheral batteries (mice, headsets) otherwise look like the system battery. Ours more: energy_now, power_now, voltage, cycles, health, AC online. |
| systemd | Not collected | D-Bus: `ListUnitsByPatterns`, `ActiveState/SubState`, `MemoryCurrent/Peak`, `CPUUsageNSec`; skip never-active; failed count | Add a systemd module via D-Bus (zbus in Rust): unit state, failed count, per-service memory/CPU, with `u64::MAX` = unavailable; poll slowly (~10 min for the list) | Gives "which service failed" which no /proc file provides. Our cgroup collector already has `cpu.stat`/`memory.current` per leaf cgroup; D-Bus adds state and names. Works unprivileged on the system bus. |
| Containers | cgroup leaves (`cpu.stat`, `memory.current`) | Docker/Podman HTTP API over a unix socket (`/var/run/docker.sock`, `/run/user/<uid>/podman/podman.sock`), `one-shot` stats; CPU as share of host; mem = `usage - inactive_file`; net summed over the container's interfaces | Keep cgroup stats (no daemon, no socket permission needed). Use the socket optionally for names, image, status/health, and per-container network. Use `usage - inactive_file` for memory | Cgroup reads are cheaper and need no docker group. The socket gives the name/image mapping and per-container net, which cgroups do not (container netns counters need the daemon; reading `/proc/<pid>/net/dev` of another user's process probably needs root, unverified). Check both Docker and rootless Podman paths. |
| Container CPU scale | cgroup usage over time | % of whole host (Docker); wall*online_cpus (Podman) | Report both core-seconds/sec and % of host | Avoids the Docker-vs-Podman denominator inconsistency (beszel issue #2049). |
| SMART | sysfs model/serial/firmware/state; btrfs `error_stats` | `smartctl --scan -j` then `-a --json=c`; `-n standby` once cached; NVMe namespace fallback; root/CAP_SYS_RAWIO needed (unverified) | Keep unprivileged by default. Add optional smartctl support behind a config flag and capability check; use `-n standby`, 15 s timeout, 30-min rescan | We cannot read SMART attributes unprivileged. Beszel's standby handling and namespace fallback are worth copying if we add it. |
| mdraid | Not collected (maybe via `/proc/mdstat`) | `/sys/block/md*/md/{array_state, level, raid_disks, degraded, sync_action, sync_completed, sync_speed, mismatch_cnt}`; per-member `dev-*/state` faulty | Add: unprivileged and cheap | Degraded or resyncing arrays are a top operational signal. Clear win. |
| eMMC | Not collected | `mmcblk*/device/{pre_eol_info, life_time, device_life_time_est_typ_a/b}` | Add on embedded targets | Wear indicator without root. |
| btrfs | `error_stats` per device | Also `missing`, `allocation/*/disk_used`, `label`, `replace_target`; ioctls on mountpoint for old kernels | Add `devinfo/*/missing`, `allocation/*` and `label`; handle `replace_target=1` as not adding capacity | Our `error_stats` is already good. `missing` is the degraded indicator. |
| ZFS | Not collected | arcstats, pool `iostats`/`objset-*` kstats, CLI | Optional: read the kstat files only (no CLI, no root) | World-readable `/proc/spl/kstat/zfs/*`. |
| Package updates | none | Subprocess per distro | Skip (heavy, network, not host metrics) unless wanted | Needs package-manager DB access and network. |
| Per-process, PSI, vmstat, zram, snmp, sockstat, file-nr | collected | not collected | Keep | Ours more. Beszel is a much narrower collector. |

### Top places beszel is clearly better

1. CPU: guest/guest_nice subtraction, iowait/steal breakdown, baseline-before-validate.
2. Memory: `Total - Available`, Shmem excluded from cache, ZFS ARC handling.
3. Disk: 32-bit ms wrap, first-sample half-interval rule, sanity caps, r/w await, weighted I/O, dm label and prefix/symlink mapping of mounts to disks.
4. mountinfo parsing for bind and btrfs-subvolume de-duplication.
5. Per-NIC delta then sum; invalid-sample re-baselining.
6. Temperature sanity filtering and fallbacks.
7. systemd D-Bus, mdraid, eMMC and fuller btrfs health.
8. Battery `scope` handling.

### Top places we collect more

- System counters (ctxt, intr, procs, file-nr).
- vmstat, zram and PSI.
- Per-process and per-cgroup data.
- All block devices and all mounts.
- snmp, sockstat, wireless quality and `ip` data.
- Non-temperature hwmon inputs.
- Full power_supply data.
- Storage firmware and state.

### Pitfalls found in beszel not to copy

- Interface and fan sets cached forever (hot-plug missed).
- Prefix-based NIC exclusion (`lo*`, `bond*`); a NIC with 0 bytes at start is excluded permanently.
- Size-equality filesystem dedupe (16 MiB tolerance) can false-positive.
- Only root plus configured mounts are reported, so most filesystems are invisible without configuration.
- Per-core CPU is rounded to an integer.
- Docker CPU is a share of the whole host, and Podman uses a different formula.
- gopsutil's `/proc/1/mountinfo` first-try is silently unreadable for non-root, so behaviour differs by user (it then uses `self`).
