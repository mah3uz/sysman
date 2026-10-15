# sysman app — design

The GPUI app that shows `sysmand`'s data: what it looks like, how it is built,
and what each screen contains.

## 1. Stack

| Piece | Choice | Why |
| --- | --- | --- |
| UI framework | `gpui-pre` + `gpui-pre-platform` from crates.io, renamed to `gpui` / `gpui_platform` | A pinned snapshot of Zed's gpui (`gpui-pre 0.3.7` = zed@1a28cff), and the gpui `gpui-component` builds against |
| Components | `gpui-component` (pinned with `=`) | Virtualised sortable table, segmented tab bar, resizable panes, sidebar, menus, tooltips, client-side title bar, themes |
| Charts, treemap, energy flow, core bars | Custom elements on `canvas` + `PathBuilder` | `gpui-component` charts lack monotone cubic, min–max band, labelled dashed rules, gaps and a shared cursor |
| Fonts | Adwaita Sans (UI, tabular digits), Adwaita Mono (paths, ids) | Clean, Inter-derived, widely installed |
| Data | `schema` crate types; socket client; read-only SQLite | Same types as the daemon, so a format change breaks the build, not the screen |

Guard against two gpui copies: CI step `cargo tree -d --depth 0 -p sysman | grep -q gpui && fail` (without `--depth 0` the listing names gpui as a parent of other duplicates).

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
  appends to per-series rings (the last 30 min at 2 s). It reconnects with
  backoff when the daemon restarts, and shows "Collector not running" instead
  of stale numbers.
- **History path.** Every chart asks `History` for `(series, from, to, width)`
  and gets min/avg/max buckets, about 120 per plot. The tier is chosen by
  range: raw `points` up to 6 h (bucketed in SQL with `GROUP BY t / bucket`),
  `rollup_1m` up to 7 days, `rollup_15m` beyond. Live rings top up the newest
  end, so a history chart reaches "now".
- **Refresh.** Charts redraw on collector ticks (2 s) only for the visible tab;
  hidden tabs are unmounted, so they cost nothing. A refresh control (2 s, 5 s,
  10 s, 30 s, 1 min) throttles redraws only; it never changes collection.

## 3. Shell

- Client-side title bar holding a centred segmented tab strip and, on the
  right, the refresh control. Under Hyprland tiling, the title bar still drags
  floating windows and shows no shadow or inset when tiled.
- Tabs and Lucide icons: Dashboard `gauge`, Processes `list`, GPU `monitor`,
  Energy `zap`, Network `network`, Disk `hard-drive`, Hardware `cpu`, Explorer
  `activity`, Insights `lightbulb`, Groups `layers`. Disk Map is a sub-tab of
  Disk.
- When the window is narrower than the strip, labels collapse to icons with
  tooltips.
- Default 1280×720, minimum 860×520.

## 4. Visual tokens

Dark first; light is derived from the same tokens.

| Token | Dark | Light |
| --- | --- | --- |
| window | `#21232F` | `#F5F5F7` |
| panel fill | white 5% over window | black 4% |
| card fill / hover | white 5% / 8% | black 4% / 6% |
| hairline | white 8%, 0.5 px | black 8% |
| text / secondary / tertiary | white 92% / 60% / 38% | black 88% / 55% / 35% |
| green orange red blue teal purple pink indigo | `30D158 FF9F0A FF453A 0A84FF 40C8E0 BF5AF2 FF375F 5E5CE6` | `34C759 FF9500 FF3B30 007AFF 30B0C7 AF52DE FF2D55 5856D6` |

Metric colours: CPU green with 60/85 bands, memory pressure orange, app memory
blue, cache teal, kernel purple, swap indigo, download green, upload red, disk
read blue, write orange, CPU temperature orange, GPU temperature red.

Typography (Adwaita Sans, tabular digits for every number): panel title 13
semibold; card label 10 semibold uppercase, secondary; card value 15 semibold;
captions 10–11 secondary; axis labels 9. Spacing: page padding 20, column gap
16, panel padding 16, card padding 11×13, rail width 300. Radii: panel 12, card
10, sub-panel 8, capsule full.

## 5. Charts

One `TrendChart` element used everywhere:
- monotone cubic mean line (width 2) that never overshoots, a faint
  straight-edged min–max band in the same hue, optional vertical gradient fill
  (alpha 0.42 → 0.04), gaps never interpolated;
- dashed labelled threshold rules (`[4,3]`, 1 px, 40% alpha);
- y axis on the left in a 56 px gutter, 4–6 ticks, unit-aware labels; x axis
  clock labels (HH:mm), or relative ("30m … now") in the process inspector;
- hover cursor with tooltip; charts in one cursor group share the cursor
  (Explorer, process inspector);
- reduction per metric: mean for rates and utilisation, max for temperatures
  and fans, last for states and counters;
- paths are cached per (data version, size), so a 2 s tick rebuilds only the
  charts whose data changed.

Sparkline (card strip, 57 px, 1.5 px line, end dot), core bars, stacked bar,
treemap and energy flow are separate small elements.

## 6. Screens

### 6.1 Dashboard
- Header: CPU model; "16 cores · 32 threads · 2 CCDs · 128 GB"; uptime and boot
  time. Range picker 5 min / 30 min / 1 hr / 6 hr / 24 hr / 7 day, remembered
  per tab.
- Six cards: PRESSURE (index, §7), AVAILABLE (`available_bytes`), APP
  (`anon_bytes`), CACHED (`cache_bytes`), COMPRESSED (zram `compressed_bytes`
  when zram exists, else KERNEL = slab + stacks + page tables), SWAP.
- Panels: Memory pressure (rules 34 Warning / 67 Critical); Processor (rules 60
  Busy / 85 Heavy; footer LIVE TOTAL, one LIVE value per CCD or core type, LIVE
  LOAD (1 MIN)); Network (down/up); Disk (read/write).
- Rail: CPU cores (one bar per logical CPU, coloured by CCD or by core type on
  Intel hybrid), Memory composition (Kernel / App / Compressed / Cached / Free,
  plus "Other" so the bar sums to total), Swap, Thermals (CPU `temp_c` orange,
  GPU `temp_c` red), Top CPU, Top disk.

### 6.2 Processes
- Header cards: CPU USAGE (user/sys split, peak), CORES, LOAD AVERAGE, CPU
  TEMPERATURE.
- Table, from socket `processes` every refresh (the full table): Process (icon
  from the `.desktop` file matched by executable, else a generic glyph), Memory
  labelled RSS (counts shared pages in full), CPU (% of one core), Threads,
  FDs, User, PID. Default sort Memory descending. Search filter; Hierarchy
  toggle (tree by ppid, flattened into rows); selection kept by pid + start
  time across re-sorts.
- Inspector: name, PID, user, start time, command line; range picker; charts
  Memory, CPU, File descriptors, Disk I/O from `proc_points` /
  `proc_rollup_1m`, sharing a cursor. Earlier runs of the same command can be
  overlaid.
- Context menu: Copy name/PID/command line; End process (SIGTERM) and Force end
  (SIGKILL), each confirmed; disabled for other users' processes.
- Values the collector may not read (other users' I/O and FDs) show "–" with a
  "not permitted" tooltip, never 0.

### 6.3 GPU
- Device picker when there is more than one GPU.
- Cards: GPU % (green by level), POWER, VRAM, TEMPERATURE, CLOCK, VIDEO
  (encode/decode or `video_busy_pct`).
- Panels: utilisation (green area), VRAM used, power with the power-limit rule.
- Rail: Device (name, driver, backend, PCIe current/max, power limit, fan,
  power state, throttle reasons as capsules); Processes (name, util, VRAM); By
  category, classified from a small name list (ollama, llama, python with torch,
  compositors, media tools).

### 6.4 Energy
- With a system battery: charge, runtime estimate from `energy_wh / power_w`,
  draw, health from `energy_full / energy_full_design`, cycles, and a flow
  diagram charger → battery → system → top consumers.
- Without one: cards CPU POWER (RAPL when readable, else hidden), GPU POWER per
  device, and the flow diagram from "AC" to CPU/GPU to top consumers (consumer
  share = CPU share, labelled as such).
- Rail: Accessories (`scope == Device` supplies: mice, headsets).
- Collector gap: `energy_full_design` is not collected yet (needed for health).

### 6.5 Network
Header "Primary: <iface> · Router <gateway>"; big DOWNLOAD/UPLOAD; throughput
chart; session totals; Adapters (glyph, name, Primary/Connected/No address
capsules, IPv4/IPv6, MAC, rates, Wi-Fi signal); Configuration (host name,
default routes, DNS); rail with TCP and sockets. Per-app traffic is not
available without root.

### 6.6 Disk and Disk Map
- Disk: device picker; cards READ, WRITE, IOPS, FREE SPACE; panels Throughput,
  Operations and latency (await, busy % with 60/90 tint, queue depth); Devices
  list (model, serial, firmware, state). Rail: Health (NVMe temperature from
  hwmon, controller state, btrfs error counters and missing devices; SMART wear
  needs root), Free space per filesystem, Top disk processes.
- Disk Map: an in-app scanner (background threads, `openat`/`statx`, stays on
  one filesystem, skips unreadable directories and counts them); squarified
  treemap; modes Map / Largest / Oldest / Kinds; one colour per file kind;
  detail rail with path, size, modified time; actions Open, Show in file
  manager (`xdg-open`), Move to Trash (freedesktop trash spec).

### 6.7 Hardware
Left outline: Overview, System (DMI, kernel, distro), Processor (topology, CCDs,
caches, frequency limits), Graphics, Memory, Storage, Network, Power, Sensors,
PCI, USB. Overview cards in a two-column grid. PCI and USB inventories are read
by the app from `/sys/bus/{pci,usb}/devices` (names via `/usr/share/hwdata`).
Sensors: a grid of mini charts, one per hwmon chip group, hottest sensor
headline, colour from green to red by the sensor's own `max`/`crit` limits.

### 6.8 Explorer
Metric sidebar built from the `series` table, grouped by module, with friendly
names for known paths; process search over `procs` (including exited); chart
grid (3 columns) with a shared cursor; Earlier/Later, go-to-time, zoom, Live;
inspector with values at the cursor and window statistics; CSV export of the
visible range.

### 6.9 Insights
"What's happening" callouts from rules over recent history: swap climbing,
sustained high CPU process, memory growth (from `proc_rollup_1m` slope), OOM
kills (`oom_kills_total` rising), thermal throttling (GPU throttle reasons).
Pressure events chart (last 2 h). Top consumers by average or peak over
1h/6h/24h/7d from `proc_rollup_1m`, grouped by command.

### 6.10 Groups
User-defined groups of processes matched by command name; combined memory and
CPU over time from `proc_points`. Suggested groups from systemd cgroups
(app-*.scope, services) shown as one-click presets. Stored in
`$XDG_CONFIG_HOME/sysman/groups.toml`.

## 7. Memory pressure index

Index (0–100) from PSI `pressure.memory.some.avg10`, piecewise linear:
0 → 0, 5 % → 34, 20 % → 67, 50 % → 100; then raised to at least 34 while swap-in
exceeds 1 MiB/s. Bands: under 34 normal (green), 34–66 warning (orange), 67+
critical (red). The breakpoints are a starting point to tune against real
slowdowns.

## 8. Milestones

1. Shell, theme, data layer, `TrendChart`, Dashboard. Done.
2. Processes (table, inspector, actions). Done.
3. GPU, Network, Disk. Done.
4. Energy, Hardware.
5. Explorer, Insights, Groups.
6. Disk Map.

Each milestone ends with the screen running against the real daemon and
checked by screenshot.

## 9. Collector changes the app needs

- `power`: add `energy_full_design` / `charge_full_design` (battery health).
