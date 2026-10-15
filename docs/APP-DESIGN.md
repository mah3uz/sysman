# sysman app — design

The GPUI app that shows `sysmand`'s data. It must match the look and behaviour
of Zesty0wl/mac-performance-monitor; that app is described in
`docs/research/mac-performance-monitor.md` (cited below as "ref §n") and shown
in `docs/research/mpm/*.png`. This document says what we build, where it
differs from the reference, and why.

## 1. Stack

| Piece | Choice | Why |
| --- | --- | --- |
| UI framework | `gpui-pre` + `gpui-pre-platform` from crates.io, renamed to `gpui` / `gpui_platform` | Snapshot of the same Zed revision `insensical` pins (`gpui-pre 0.3.7` = zed@1a28cff), and the only gpui `gpui-component` builds against |
| Components | `gpui-component` (pinned with `=`) | Virtualised sortable table, segmented tab bar, resizable panes, sidebar, menus, tooltips, client-side title bar, themes |
| Charts, treemap, energy flow, core bars | Custom elements on `canvas` + `PathBuilder` | `gpui-component` charts lack monotone cubic, min–max band, labelled dashed rules, gaps and a shared cursor (`docs/research/gpui-stack.md`) |
| Fonts | Adwaita Sans (UI, tabular digits), Adwaita Mono (paths, ids) | Installed; Adwaita Sans is Inter-derived, the nearest to SF Pro |
| Data | `schema` crate types; socket client; read-only SQLite | Same types as the daemon, so a format change breaks the build, not the screen |

Guard against two gpui copies: CI step `cargo tree -d -p sysman | grep -q gpui && fail`.

## 2. Architecture

```
crates/sysman/src/
  main.rs            app + window
  data/live.rs       socket client thread: subscribe → Live entity (latest record + in-memory ring per series, 30 min)
  data/history.rs    read-only history.db; queries run on the background executor
  data/process.rs    socket `processes` + procs/proc_points queries
  ui/shell.rs        title bar, tab strip, refresh control; mounts only the active tab
  ui/theme.rs        tokens (§4)
  ui/widgets/        metric card, panel, range picker, legend, key/value list, capsule badge
  charts/            trend chart, sparkline, core bars, stacked bar, treemap, energy flow
  screens/           one module per tab
```

- **Live path.** One background thread holds a `subscribe` connection, parses
  each line into `schema::Record`, and pushes it to a `Live` entity, which
  appends to per-series rings (the last 30 min at 2 s). Reconnects with backoff
  when the daemon restarts, and shows "Collector not running" instead of stale
  numbers.
- **History path.** Every chart asks `History` for `(series, from, to, width)`
  and gets min/avg/max buckets sized to about 120 per plot (ref §3.4). The tier
  is chosen by range: raw `points` up to 6 h (bucketed in SQL with
  `GROUP BY t / bucket`), `rollup_1m` up to 7 days, `rollup_15m` beyond. Live
  rings top up the newest end, so a history chart reaches "now".
- **Refresh.** Charts redraw on collector ticks (2 s) only for the visible tab;
  hidden tabs are unmounted (ref §2 TabGate). A refresh control (2 s, 5 s,
  10 s, 30 s, 1 min) throttles redraws only; it never changes collection.

## 3. Shell

- Client-side title bar holding a centred segmented tab strip and, on the
  right, the refresh control. No traffic lights. Under Hyprland tiling, the
  title bar still drags floating windows and shows no shadow or inset when tiled.
- Tabs and icons (Lucide, nearest to the reference's SF Symbols): Dashboard
  `gauge`, Processes `list`, GPU `monitor`, Energy `zap`, Network `network`,
  Disk `hard-drive`, Hardware `cpu`, Explorer `activity`, Insights `lightbulb`,
  Groups `layers`. Disk Map is a sub-tab of Disk, as in the reference (ref §2.6).
- When the window is narrower than the strip, labels collapse to icons with
  tooltips.
- Default 1280×720, minimum 860×520 (ref §1).

## 4. Visual tokens

Dark is the reference; light is derived from the same tokens.

| Token | Dark | Light | Source |
| --- | --- | --- | --- |
| window | `#1E1E20` | `#F5F5F7` | ref §3.1, neutral variant |
| panel fill | white 5% over window | black 4% | ref `.quaternary.opacity(0.35)` |
| card fill / hover | white 5% / 8% | black 4% / 6% | ref MetricCard |
| hairline | white 8%, 0.5 px | black 8% | ref §3.1 |
| text / secondary / tertiary | white 92% / 60% / 38% | black 88% / 55% / 35% | |
| green orange red blue teal purple pink indigo | `30D158 FF9F0A FF453A 0A84FF 40C8E0 BF5AF2 FF375F 5E5CE6` | Apple light values | ref §3.1 |

Metric colours follow ref §3.1 exactly: CPU green with 60/85 bands, memory
pressure orange, app memory blue, cache teal, kernel purple, swap indigo,
download green, upload red, disk read blue, write orange, CPU temperature orange,
GPU temperature red.

Typography (Adwaita Sans, tabular digits for every number): panel title 13
semibold; card label 10 semibold uppercase, tracking 0.6, secondary; card value
15 semibold; captions 10–11 secondary; axis labels 9. Spacing: page padding 20,
column gap 16, panel padding 16, card padding 11×13, rail width 300. Radii:
panel 12, card 10, sub-panel 8, capsule full. (ref §3.2–3.3)

## 5. Charts (ref §3.4)

One `TrendChart` element used everywhere:
- monotone cubic mean line (width 2), translucent min–max band in the same hue,
  optional vertical gradient fill (alpha 0.42 → 0.04), gaps never interpolated;
- dashed labelled threshold rules (`[4,3]`, 1 px, 40% alpha);
- y axis on the left in a 56 px gutter, 4–6 ticks, unit-aware labels; x axis
  clock labels (HH:mm), or relative ("30m … now") in the process inspector;
- hover cursor with tooltip; charts in one `CursorGroup` share the cursor
  (Explorer, process inspector);
- reduction per metric: mean for rates and utilisation, max for temperatures
  and fans, last for states and counters.
- Paths are cached per (data version, size) so a 2 s tick rebuilds only the
  charts whose data changed.

Sparkline (card strip, 57 px, 1.5 px line, end dot), core bars, stacked bar,
treemap and energy flow are separate small elements.

## 6. Screens

Status column: **=** same as the reference, **~** adapted (reason given), **+** new.

### 6.1 Dashboard (ref §2.1, [01])
- Header: CPU model; "16 cores · 32 threads · 2 CCDs · 128 GB"; uptime and boot
  time. Range picker 5 min / 30 min / 1 hr / 6 hr / 24 hr / 7 day, remembered per tab. **=**
- Six cards **~**: PRESSURE (index, §7), AVAILABLE (`available_bytes`; Linux's
  meaningful "free"), APP (`anon_bytes`), CACHED (`cache_bytes`), COMPRESSED
  (zram `compressed_bytes` when zram exists, else KERNEL = slab + stacks +
  page tables), SWAP.
- Panels: Memory pressure (rules 34 Warning / 67 Critical), Processor (rules 60
  Busy / 85 Heavy; footer LIVE TOTAL, one LIVE value per CCD instead of P/E
  **~**, LIVE LOAD (1 MIN)), Network (down/up), Disk (read/write).
- Rail: CPU cores (one bar per logical CPU, coloured by CCD or by core type on
  Intel hybrid **~**), Memory composition (Kernel / App / Compressed / Cached /
  Available, plus "Other" so the bar sums to total **~**), Swap, Thermals (CPU
  `temp_c` orange, GPU `temp_c` red), Top CPU, Top disk.

### 6.2 Processes (ref §2.2, [02])
- Header cards: CPU USAGE (user/sys split, peak), CORES, LOAD AVERAGE, CPU
  TEMPERATURE. **=**
- Table, from socket `processes` every 2 s (the full table): Process (icon from
  the `.desktop` file matched by executable, else a generic glyph), Memory
  labelled **RSS** (overcounts shared pages; the reference's footprint figure
  has no Linux equivalent **~**), CPU (% of one core), Threads, FDs, **User**
  (replaces Arch **~**), PID. Default sort Memory descending. Search filter;
  Hierarchy toggle (tree by ppid, flattened into rows); selection kept by
  pid + start time across re-sorts.
- Inspector: name, PID, user, start time, command line (replaces the glossary
  card, which needs a curated database **~**); range picker; charts Memory, CPU,
  File descriptors, Disk I/O from `proc_points` / `proc_rollup_1m`, sharing a
  cursor. Earlier runs of the same command can be overlaid. **=**
- Context menu: Copy name/PID/command line; End process (SIGTERM) and Force
  end (SIGKILL), each confirmed; disabled for other users' processes. **~**
- Values the collector may not read (other users' I/O and FDs) show "–" with a
  "not permitted" tooltip, never 0.

### 6.3 GPU (ref §2.3, [03])
- Device picker when more than one GPU (this machine: RTX 4090, Raphael iGPU). **+**
- Cards: GPU % (green by level), POWER, VRAM, TEMPERATURE, CLOCK, VIDEO
  (encode/decode or `video_busy_pct`) **~** (Neural Engine cards dropped:
  no Linux equivalent).
- Panels: utilisation (green area), VRAM used, power with the power-limit rule.
- Rail: Device (name, driver, backend, PCIe current/max, power limit, fan,
  power state, throttle reasons as capsules), Processes (name, util, VRAM).
  The reference's "By category" groups processes by inferred kind; we keep a
  small name list (ollama, llama, python, blender, steam games…) **~**.

### 6.4 Energy (ref §2.4, [09])
- With a system battery: the reference layout (charge, runtime estimate from
  `energy_wh / power_w`, draw, health from `energy_full / energy_full_design`,
  cycles, flow diagram charger → battery → system → top consumers). **=**
- Without one (this desktop): cards CPU POWER (RAPL when readable, else
  hidden), GPU POWER per device, and the flow diagram from "AC" to CPU/GPU to
  top consumers (consumer share = CPU share, labelled as such). **~**
- Rail: Accessories (`scope == Device` supplies: mice, headsets).
- Collector gap: `energy_full_design` is not collected yet (needed for health).

### 6.5 Network (ref §2.5, [07])
Header "Primary: <iface> · Router <gateway>"; big DOWNLOAD/UPLOAD; throughput
chart; session totals; Adapters (glyph, name, Primary/Connected/No address
capsules, IPv4/IPv6, MAC, rates, Wi-Fi signal); Configuration (host name,
default routes, DNS). **=** Per-app traffic is not available without root **~**.

### 6.6 Disk and Disk Map (ref §2.6, [05], [06])
- Disk: device picker; cards READ, WRITE, IOPS, FREE SPACE; panels Throughput,
  Operations and latency (await, busy % with 60/90 tint, queue depth); Devices
  list (model, serial, firmware, state). Rail: Health (NVMe temperature from
  hwmon, controller state, btrfs error counters and missing devices — no SMART
  wear without root **~**), Free space chart per filesystem, Top disk processes.
- Disk Map: an in-app scanner (background threads, `openat`/`statx`, stays on
  one filesystem, skips unreadable directories and counts them); squarified
  treemap; modes Map / Largest / Oldest / Kinds; kind colours from ref §2.6;
  detail rail with path, size, modified time; actions Open, Show in file
  manager (`xdg-open`), Move to Trash (freedesktop trash spec). "Reclaim" and
  "Changes" modes are deferred **~**.

### 6.7 Hardware (ref §2.7, [08])
Left outline: Overview, System (DMI, kernel, distro), Processor (topology, CCDs,
caches, frequency limits), Graphics, Memory, Storage, Network, Power, Sensors,
PCI, USB. Overview cards in a two-column grid. PCI and USB inventories are read
by the app from `/sys/bus/{pci,usb}/devices` (names via `/usr/share/hwdata`). **~**
Sensors: a grid of mini charts, one per hwmon chip group, hottest sensor
headline, colour from green to red by the sensor's own `max`/`crit` limits. **=**

### 6.8 Explorer (ref §2.8, [13])
Metric sidebar built from the `series` table, grouped by module, with friendly
names for known paths; process search over `procs` (including exited); chart
grid (3 columns) with a shared cursor; Earlier/Later, go-to-time, zoom, Live;
inspector with values at the cursor and window statistics; CSV export of the
visible range. **=**

### 6.9 Insights (ref §2.9, [10])
"What's happening" callouts from rules over recent history: swap climbing,
sustained high CPU process, memory growth (from `proc_rollup_1m` slope), OOM
kills (`oom_kills_total` rising) **+**, thermal throttling (GPU throttle
reasons) **+**. Pressure events chart (last 2 h). Top consumers by average or
peak over 1h/6h/24h/7d from `proc_rollup_1m`, grouped by command. **=**

### 6.10 Groups (ref §2.10)
User-defined groups of processes matched by command name; combined memory and
CPU over time from `proc_points`. Suggested groups from systemd cgroups
(app-*.scope, services) shown as one-click presets **~**. Stored in
`$XDG_CONFIG_HOME/sysman/groups.toml`.

## 7. Memory pressure index (our own design)

The reference blends macOS's three-level kernel state; Linux has PSI instead.
Index (0–100) from `pressure.memory.some.avg10`, piecewise linear:
0 → 0, 5 % → 34, 20 % → 67, 50 % → 100; then raised to at least 34 while swap-in
exceeds 1 MiB/s. Bands as the reference: under 34 normal (green), 34–66 warning
(orange), 67+ critical (red). The breakpoints are a starting point to tune
against real slowdowns on this machine.

## 8. Milestones

1. Shell, theme, data layer, `TrendChart`, Dashboard.
2. Processes (table, inspector, actions).
3. GPU, Network, Disk.
4. Energy, Hardware.
5. Explorer, Insights, Groups.
6. Disk Map.

Each milestone ends with the screen running against the real daemon and a
screenshot compared side by side with the reference.

## 9. Collector changes the app needs

- `power`: add `energy_full_design` / `charge_full_design` (battery health).
- Already done or in progress: full process table on the socket, per-process
  history.
