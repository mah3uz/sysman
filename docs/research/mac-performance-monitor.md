# Mac Performance Monitor: design reference

Source: https://github.com/Zesty0wl/mac-performance-monitor (shallow clone, read only, never built). Version documented: 2.2 / 2.3 (README, Oct 2026). Native macOS 15+ app, Swift 6, SwiftUI plus AppKit, Apple silicon only, MIT.

Path shorthand: `APP/` = `Sources/MacPerfMonitor/`, `CORE/` = `Sources/MacPerfMonitorCore/`, `docs/` = repo `docs/`. Screenshots are in `/home/mahfuz/Projects/sysman/docs/research/mpm/` (16 files, all under 3 MB, none skipped). Screenshot numbers are written as [01] and so on.

Evidence levels used below: "(src)" read in source, "(shot)" read off a screenshot, "(Apple)" a standard SwiftUI system colour whose value is Apple's, not the repo's. I did not read GPUView, NetworkView, DiskUsageView, HardwareView or the Disk Map views line by line. Their layouts come from screenshots plus the repo docs, so treat their spacing as approximate.

## 1. Overview

A "performance monitor and recorder": live readout, background history recorder, and a window for investigation. Core idea is to see now, then go back to the moment a slowdown began and compare machine metrics with the processes behind them (README).

Feature list (README, docs/): Dashboard, Processes (table plus inspector, per-process history), GPU and Neural Engine, Energy (battery, power flow, accessories), Network (adapters, optional per-app traffic), Disk (throughput, IOPS, latency, SMART-like health), Disk Map (treemap), Hardware inventory, Explorer (linked charts, shared cursor, inspector, CSV/trace export), Insights (leak/growth, top consumers, pressure events), Groups (process groups with combined footprint), Alerts engine, menu bar item with popover, Ask About This Mac (on-device LLM, macOS 27 only), MCP server and `mpm` CLI for agents.

Window and navigation model
- One main window, a segmented tab strip centred in the unified-compact toolbar: Dashboard, Processes, GPU, Energy, Network, Disk, Hardware, Explorer, Insights, Groups (`APP/Views/ContentView.swift:33-51`, tab icons at `:101-150`). Tabs are mounted only while active (`TabGate`), so an inactive tab costs nothing (`ContentView.swift:57-63`, comment on the re-render storm).
- Toolbar trailing: a refresh-interval menu (timer icon plus "1s", "10s") on every tab (`APP/Views/RefreshIntervalControl.swift:26-37`) and a sparkle button (Ask) when enabled [01].
- Window size: default width `max(1280, minimumWidth+100)`, default height 720, minimum 860 x 520. Minimum width is measured from the localised tab titles (`ContentView.swift:5-30`).
- Menu bar item: one combined `NSStatusItem` with an `NSPopover`, 404 pt wide (`APP/App/CombinedStatusItemController.swift:128,226`; `APP/Views/CombinedMenuBarContentView.swift:57`). Optional notch display (`APP/App/NotchDisplayController.swift`).
- Secondary windows: Settings (780x760), Usage Timeline (800x660), Ask, trace viewer, onboarding (`APP/App/MacPerfMonitorApp.swift:150,194,242,257,271,285`).
- Dock icon follows the window; a setting keeps it pinned. App can run with window closed (recorder and menu bar separate switches, README).

Page pattern. Most history pages use `MainRailLayout`: a flexible main column plus a fixed 300 pt right "stats rail", 16 pt gap, 20 pt outer padding (`APP/Views/PanelLayout.swift:16-32`; `DashboardView.swift:66`). Pages with a header row show an uppercase caption "HISTORY" over a small segmented range picker at the top right (`DashboardView.swift:116-135`).

## 2. Screens

### 2.1 Dashboard  [01-dashboard.png]
Purpose: one-screen overview of memory pressure first (the app's "North Star"), then CPU, network, disk, thermals.

```
+------------------------------------------------------+-------------------+
| Apple M3 Pro                        HISTORY          | CPU cores   [exp] |
| 11 cores (5P+6E) . 18.0 GB memory   [5m|30m|1h|6h|   | 11 vertical bars  |
| Uptime 13h 12m                       24h|7d]         | legend P 47% E100%|
| [PRESSURE][FREE][APP][COMPRESSED][CACHED][SWAP]      +-------------------+
|  6 metric cards: dot+LABEL, value, sparkline         | Memory composition|
| +--------------------------------------------------+ | stacked bar+legend|
| | Memory pressure                          [exp]   | +-------------------+
| | line chart 0-100, dashed Warning 34/Critical 67  | | Swap  line chart  |
| | legend: Average (15 s) / Recorded range          | +-------------------+
| +--------------------------------------------------+ | Thermals CPU/GPU  |
| | Processor  0-100%, dashed Busy 60/Heavy 85       | +-------------------+
| | LIVE TOTAL | LIVE PERFORMANCE | LIVE EFFICIENCY |  | Top CPU, Top disk |
| |  | LIVE LOAD (1 MIN)      caption text           | |  (below fold)     |
| +--------------------------------------------------+ |                   |
| | Network  LIVE DOWNLOAD / LIVE UPLOAD, chart      | |                   |
| | Disk ...                                         | |                   |
+------------------------------------------------------+-------------------+
```
Source: structure `APP/Views/DashboardView.swift:46-62`; header `:116-135`; card chrome `:1176-1222`.

Widgets
- Page header: CPU brand in `.headline`, then cores subtitle, uptime and boot date (`DashboardView.swift:116-124`; README 2.1).
- Range picker: segmented, `.small`: 5 min, 30 min, 1 hr, 6 hr, 24 hr, 7 day (`CORE/Persistence/HistoryQuery.swift:8-28`). Each view remembers its own range; history views start at 30 min (README 2.2).
- Six metric cards (`APP/Views/MetricCard.swift`): Pressure (%), Free, App, Compressed, Cached Files, Swap. Each: 6 pt tint dot, uppercase caption label, `info.circle` glyph, value, trailing "Axis max N" caption, a 57 pt sparkline strip (`MetricCard.swift:158,225-299`). Click opens a detail sheet with a 300 pt axed chart, selected-range stats and a "what this means / how it is calculated" explanation (`MetricCard.swift:560-600`).
- Panel charts (Memory pressure, Processor, Network, Disk, Swap, Thermals): panel title with SF Symbol plus expand arrow. See section 3 for chart styling. Threshold rules are dashed labelled lines: pressure 34 "Warning" (orange) and 67 "Critical" (red) (`DashboardView.swift:929-930`); CPU 60 "Busy" orange and 85 "Heavy" red (`:962-963`).
- Processor footer: LIVE TOTAL (tinted by CPU level), LIVE PERFORMANCE (blue), LIVE EFFICIENCY (teal), LIVE LOAD (1 MIN). Caption: "Live values are smoothed... Per-process CPU uses one core as 100%" [01].
- Rail: CPU cores (one vertical bar per logical core, coloured by cluster, tooltip with percent and core type), Memory composition (stacked bar plus legend rows "name / GB . %"), Swap, Thermals (CPU orange, GPU red), Top CPU, Top disk (`DashboardView.swift:55-61`).
- Legend row under charts: "Average (15 s)" line swatch and "Recorded range" translucent swatch [01].
- Memory pressure index: 0-100 derived from the kernel's normal/warning/critical level (bands 0-33, 34-66, 67-100) blended with compression and swap signals (`docs/pressure-index.md`).

Refresh: live ticks append to the in-memory window; hidden-window samples are still collected, only drawing is skipped, so gaps always mean "no data" (`DashboardView.swift:81-93`). Rankings follow the table cadence (`:76-79`). Default refresh 10 s, choices 250 ms to 5 min (section 4.7).

Interactions: hover shows a gap-aware tooltip and a vertical cursor; card or panel click opens a frozen snapshot detail sheet (`DashboardDetailSheet.swift`); hover on a card raises its fill and tints its border (`MetricCard.swift:301-311`).

### 2.2 Processes  [02-processes.png]
Purpose: live table of all processes plus an inspector with per-process history.
```
+------------------------------------------------------+----------------------+
| PROCESSOR                                  11 cores  | [icon] Code Helper   |
| [CPU USAGE 27%][CORES bars][LOAD AVG 8.47][CPU DIE]  |  PID 1861            |
| 576 processes                                        | purple glossary card |
| (search) Filter by name                [Hierarchy]   | [5m|30m|1h|6h|24h|7d]|
| Process        Memory v  CPU Threads FDs Arch   PID  | Memory footprint ch. |
| [ic] name      2.1 GB   8.5%   35    71  arm64 1861 | CPU chart (% of core)|
| ...  ~26 px rows, zebra banding, selected = grey     | File descriptors     |
+------------------------------------------------------+ Disk I/O ...         |
```
Source: `APP/Views/ProcessListView.swift`, `ProcessOutlineTable.swift:146-200` (columns), `ProcessDetailView.swift`, `ProcessesTab` in `ContentView.swift`.
- Header cards: four `MetricCard`s (CPU usage with "12% user . 13% sys" and peak, Cores bar strip with P/E legend, Load average with 5 and 15 min, CPU die temperature) [02].
- Table (AppKit `NSOutlineView`, so rows do not re-layout each tick): columns Process (min 160, ideal 260), Memory (84/104, right, default sort descending), CPU (58/70), Threads (60/72), FDs (50/62), Arch (60/72), PID (52/72). Memory is `phys_footprint`, not RSS. CPU is percent of one core, can exceed 100. Process icons come from the app bundle (`Util/ProcessIconProvider.swift`). Warning triangle and orange name for a flagged (growing) process [02].
- Search filter and a "Hierarchy" toggle (tree by parent PID, persisted `processShowHierarchy`, launchd hidden) (`ProcessListView.swift:25-30`). Multi-select with cmd/shift click.
- Inspector: name, PID, a glossary card explaining what the process is (from signed `Resources/glossary.json`, `Catalog/ProcessGlossaryStore.swift`), range picker, then charts: Memory footprint, CPU, File descriptors, Disk I/O read/write, network if per-app tracking is on. Earlier runs of the same program can be overlaid (README 2.1).
- Row context menu: open detail, Usage Timeline, code-signing sheet, reveal in Finder, force quit with confirm (`Util/ProcessActions.swift`, `ProcessOutlineTable.swift:537`).
Refresh: full process scan every 5 s minimum; rows on screen re-read at the dial rate in between (`RefreshIntervalControl.swift:3-10`, `CORE/Util/LiveRefreshCadence.swift:12`).

### 2.3 GPU  [03-gpu.png, 04-gpu-neural-engine.png]
Purpose: device and per-process GPU use, power, memory, Neural Engine.
- Header: chip name and "14-core GPU . unified memory", range picker. Six cards: GPU % (green, level-coloured), GPU POWER W (blue), GPU MEMORY (teal), GPU AWAKE % (orange), ANE TIME ms/s (purple), ANE POWER W (pink) (`APP/Views/GPUView.swift:804-838`).
- Panels: GPU utilization (filled green area, 0-100, then DEVICE / RENDERER / TILER / GPU awake stats), GPU memory bandwidth (three lines Total teal, Reads blue, Writes orange, tagged "Preview" because values are estimates), Neural Engine activity.
- Rail: Device (key/value list: chip, cores, memory in use, allocated, die temperature, fan, thermal limit, power cap, GPU recoveries), Clock states (four horizontal bars OFF/P1/P2/P3), By category (stacked bar AI and ML purple, Display and UI blue, Media teal, Other grey), AI workloads (process list with dot, name, category, percent, memory; recognises Ollama, MLX, LM Studio) [03].
- Per-process table reuses `ProcessTable` with GPU columns: share of GPU, GPU time per second, idle time, category (`ProcessOutlineTable.swift:152-200`).

### 2.4 Energy  [09-energy.png]
- Six cards: CHARGE, ESTIMATED RUNTIME, MAC POWER DRAW, BATTERY TEMPERATURE, HEALTH, CYCLES. The first four follow the page range; Health defaults to 90 days and Cycles 1 year (`docs/energy-design.md`). Card footer shows its own range ("30 min", "90 days").
- Energy flow panel: animated node graph. Left: charger (blue, "96 W max") and battery (green, "holding"). Centre: laptop node with charge %. Right: top energy-consuming processes (icon, name, share %) joined by orange curves with travelling dots (`APP/Charts/EnergyFlowView.swift`). Caption "On power adapter . battery holding".
- Then Charge over time (dashed forecast line kept separate from recorded data), electrical panel (voltage, amps, cell voltages), top energy processes.
- Rail: Accessories (mouse, AirPods with Left/Right/Case, battery glyph, bolt when charging, "Checked 13:31"), Battery health (89% in orange, cycle count, condition, manufacturer, full charge mAh, design mAh) [09].
- Colours: charger blue, battery green, consumers orange (`APP/Util/BatteryStyle.swift:53-57`). Battery level: under 20 critical red, under 40 low orange, else green (`:15-25`).
- Runtime: macOS estimate first, else fallback from at least 3 min of steady discharge in the last 5 min, at most 128 readings, void beyond 7 days (`docs/energy-design.md`).

### 2.5 Network  [07-network.png]
Note: this screenshot is from an older build (no GPU/Hardware tabs).
- Header line "Primary: USB 10/100/1000 LAN . Router 192.168.68.1" left; right big DOWNLOAD (green arrow.down) and UPLOAD (red arrow.up) values, caption uppercase below.
- Throughput panel: two-line chart (green down, red up), auto-scaled axis in B/s, KB/s, MB/s. Footer "This session: 400.4 MB downloaded . 164.5 MB uploaded".
- Adapters panel: rows with glyph (cable, wifi, bridge), name, bsd name in small caps, capsule badges "Primary" (blue), "Connected" (green), "No address" (grey); IPv4 and MAC lines; right side down/up rate and totals. Disconnected adapters dimmed. Chevron opens `NetworkAdapterDetailView`.
- Configuration panel: key/value rows: computer name, primary service, router, DNS servers, search domains. Optional per-app traffic (Settings), network scan tool (`NetworkScanView.swift`).
- Colours: `NetworkStyle.download = .green`, `.upload = .red` (`APP/Util/NetworkStyle.swift:8-11`); symbols `arrow.down`, `arrow.up`.

### 2.6 Disk and Disk Map  [05-disk.png, 06-disk-map.png]
Disk page (`APP/Views/DiskUsageView.swift`, `DiskDetailModel`): header with device model and "465.9 GB, Apple Fabric, internal"; range 1h/6h/24h/7d; four cards READ (blue), WRITE (orange), IOPS (white/grey), FREE SPACE (line, "55.7 GB of 460.4 GB"); Throughput panel (read blue, write orange, light area under); Operations and latency panel (READ LATENCY blue, WRITE LATENCY orange, UTILIZATION % with red bar, ops/s chart, service time chart); Devices panel (rows with protocol, serial, medium, controller, live R/W/IOPS/latency/Busy). Rail: Health (Verified tick, temperature, life used bar, available spare, power-on hours, cycles, unsafe shutdowns, media errors, lifetime reads/writes, driver error counters), Free space chart plus Purgeable, Top disk processes (icon, name, MB/s) [05]. Utilisation tint: grey below 60, orange 60-89, red 90 and up (`APP/Util/DiskStyle.swift:12-14`). Read blue, write orange (`:7-8`).

Disk Map (`APP/Views/DiskMap/*`, design in `docs/disk-map-design.md`): sub-tabs Overview / Disk Map; toolbar: volume picker, Rescan, "Scanned 14 seconds ago", mode tabs Map / Largest / Oldest / Kinds / Reclaim / Changes, Colour picker (Kind). Reconciliation bar (blue scanned, grey unaccounted, teal macOS, track free) with count pills ("25 not permitted" in orange, "32 protected by macOS"). Squarified treemap (`CORE/DiskMap/TreemapLayout.swift`), folder headers with size at right, tiles labelled "name / size", "1,483 more" aggregate tile, legend of kind colours at the bottom. Right detail rail: file icon, path with copy button, Size, Length, Would free now, Share of scan/parent, Modified, advice box ("Review before removing . Your files"), buttons Reveal in Finder, Quick Look, Move to Trash. Kind palette: `APP/Util/DiskMapStyle.swift` (not read in detail; see [06] legend: Folders grey, Applications blue, Video purple, Images orange, Audio pink, Archives teal, Documents yellow, Code purple-blue, Data blue, Logs orange-brown, System grey).
Capacity bar colours are fixed five-slot hex values with light/dark pairs, see 3.1.

### 2.7 Hardware  [08-hardware.png]
Source `APP/Views/HardwareView.swift`, `HardwareOverviewView.swift` (1001 lines), `CORE/Hardware/*`.
- Left outline sidebar (about 380 px): Overview plus 20 categories with count badges: Mac, Processor, Graphics, Memory, Displays, Storage, Power and battery, Sensors, Network, Wi-Fi, Bluetooth, USB, Thunderbolt and USB4, Audio, Cameras, PCI, Peripherals, Printers and scanners, Security, Software. Each expands to nodes.
- Top bar: title and model subtitle, search field "Search hardware", "Updated 15:56:56 / 4.4 s", Refresh, Export menu (saved report).
- Overview cards in a 2-column grid, each with icon, title and "Details >" link: This Mac, System on a chip (CPU/GPU/Neural Engine mini-cards with dot grids per core, unified memory strip, legend), Memory (bar plus type), Storage (volume bars), Displays (thumbnail rectangles, resolution, Built-in/External), Battery (ring gauge), Connectivity, Software (kernel, last boot, uptime), Sensors.
- Sensors panel: 3-column grid of 11 mini line charts (CPU die P cores, E cores, GPU clusters, SSD, Battery, Airflow, Skin and board, Wireless, Voltage rails, Other, Fans). Each shows the hottest sensor of its domain over the last 5 minutes, live; header with icon, name, current value coloured by temperature (green to yellow). Click shows every sensor in the group. Hardware is a snapshot, refreshed on demand (README).

### 2.8 Explorer (labelled "Analytics" in older builds)  [13-explorer.png, 11-analytics.png]
Source `APP/Views/AnalyticsView.swift`, `DataExplorerView.swift`, `ViewModels/DataExplorerModel.swift`, `Charts/ExplorerCursor.swift`, `docs/explorer-design.md`.
```
| Explorer [<][>][1 hr v][cal][zoom+][zoom-][ Live ]   [list][refresh][split][...] |
| [Metrics|Processes]  | 10 Sep 2026 at 13:45:10 to 14:45:10     | [Values|Procs|Hardware] |
| Filter metrics       | [Total CPU ][Process CPU][Pressure idx] | At cursor  |<  >| pin |
| Views v              | [Mem comp. ][Mem footprint][Net thrput] | Total CPU 14.2%        |
| Processor            | [Phys disk ][Die temps   ][Load avg  ]  | Window statistics      |
|  [x] Total CPU ...   | [Swap      ][IOPS        ][Disk util ]  | avg / min / max        |
| (23 machine, 19 proc)| 3-column chart grid, 12 shown           | 2,154 recorded samples |
| Following live  12 charts                                       14:45:10         |
```
- Metric sidebar: checkbox list grouped Processor, Memory, Network, Disk, GPU, Sensors, Energy, Processes. Selecting adds a chart tile. A "Processes" sidebar tab searches names and PIDs including exited processes; up to 8 processes compared, each with a stable colour (docs/explorer-design.md).
- Chart tile: icon, title, scope tag ("Machine" or "Processes") and expand arrow, 3-col grid, legend underneath.
- Shared cursor across all charts; click pins; Command+scroll zooms around pointer; Earlier/Later buttons; "Go to time"; Resume live; CSV export of visible data; `.mpmtrace` process-trace export (`APP/Export/TraceExport.swift`).
- Inspector tabs Values (value, time, min/max, "Recorded machine row"), Processes (ranked by CPU at the pinned time), Hardware.
- Process-only analytics view [11]: purple line charts for one process: Memory footprint, CPU (percent of one core), Network, File descriptors, Disk I/O; range 2m/1h/6h/24h/7d; list of selected processes with colour swatch, PID, current value, remove button.

### 2.9 Insights  [10-insights.png]
Source `APP/Views/InsightsView.swift`, `CORE/Analysis/InsightEngine.swift`.
- "What's happening": up to two coloured callout cards with 28 px rounded icon tile, title, value at right, explanation ("Swap is climbing +2.4 GB" orange tint, "X is using a lot of CPU 90%" blue tint).
- Pressure events: last 2 hours, filled orange area with Warning/Critical dashed rules, markers for each spike, fallback text "No pressure events...".
- Top consumers: window picker 1h/6h/24h/7d at left, metric picker Average/Peak/CPU/Energy/Network at right; ranked rows: index, icon, name, thin blue bar proportional to value, value at right, caption "peak 5.1 GB . 33.7% CPU . 2239 samples".
- Also leak/growth board, Rosetta list (`Views/LeakIndicator.swift`, `Persistence/LeakBoard.swift`).

### 2.10 Groups
`APP/Views/GroupsView.swift`, `GroupEditorView.swift`, `GroupDetailView.swift`, `CORE/Analysis/GroupFootprint.swift`. User-defined sets of apps and helpers; combined memory footprint and share of RAM over time. No screenshot.

### 2.11 Usage Timeline sheet  [12-usage-timeline.png]
Header icon plus title and PID. Range 30m/1h/6h/24h/7d, prev/next/refresh icon buttons, "Ending" date picker. Three horizontal swim lanes with 1-minute bucket bars: Sampled running (blue), App usage (green), Media usage (orange); time axis at 5 min ticks; legend with totals; disclaimer "Filled running buckets contain a sample, not proof of continuous execution"; checkbox "Include Apple app activity". Source `APP/Views/UsageTimelineView.swift`, `CORE/Analysis/UsageTimeline.swift`.

### 2.12 Ask About This Mac  [14-ask-preview.png, 15-display-capture-load.png]
Light-mode window: title with sparkle icon and "Preview" tag, reset and settings buttons; left list of canned questions as blue links (Why is my Mac slow?, What is using my CPU?...); right report with rows "title / explanation / value / open-in-chart arrow", "Limits" section, "Open evidence" button; bottom text field and send button; footer "On-device AI is off. Current reports remain available". Needs macOS 27 and Apple Foundation Models (`APP/Ask/*`, `docs/ask-redesign.md`). [15] is a small banner image for the display-capture-load insight (`docs/display-capture-load.md`).

### 2.13 Menu bar item and popover  [16-menubar-icon.png]
- Status item shows a strip of selected metrics (or one focused metric): each cell is a 44 pt tall button with short label (RAM, CPU, GPU, BAT, TMP, NET, DSK), value in `.caption.semibold.monospacedDigit`, a 4 pt selection dot, and a red `exclamationmark.triangle.fill` when the metric is in alarm; selected cell fill is `accentColor` at 0.16; container is 6 pt-radius with `controlBackgroundColor` fill and 0.22 secondary stroke (`APP/Views/CombinedMenuBarContentView.swift:80-130`). Modes: Focus or Strip, default Strip; at least one metric must stay selected (`APP/App/CombinedMenuBarConfiguration.swift:52-110`).
- Popover shows the selected panel: pressure summary, CPU, GPU, Energy, Temperature, Network, Disk (`APP/Views/*MenuBarContentView.swift`), plus an alerts badge panel (`AlertsMenuBarContentView.swift`). Metric icons: memorychip, cpu, display, bolt.fill, thermometer.medium, network, internaldrive (`CombinedMenuBarConfiguration.swift:42-48`).
- Menu bar rendering is bitmap-based (`App/MenuBarReadoutImage.swift`, `CombinedMenuBarImage.swift`, `Charts/MenuBitmapChart.swift`) and the trend strip uses LED-style activity dots for network (green down, red up).
- [16] is the 571x412 template icon.

## 3. Visual system

### 3.1 Colour
The app uses SwiftUI system colours almost everywhere, so values adapt to light/dark and to the user accent. Only the Disk capacity bar uses literal hex (src `APP/Util/DiskStyle.swift:21-27`). Values in the table for system colours are Apple's dark-mode sRGB (Apple, not from the repo).

| Name | Dark hex (Apple) | Light hex (Apple) | Used for (src) |
| --- | --- | --- | --- |
| green | 30D158 | 34C759 | Processor line, light CPU level, normal pressure, download, GPU util, battery, health OK (`CPUStyle.swift:20`, `PressureStyle.swift:7`, `NetworkStyle.swift:8`) |
| orange | FF9F0A | FF9500 | Memory pressure line, compressed, busy CPU, warning, disk write, consumers (`PressureStyle.swift:8,28`, `DiskStyle.swift:8`) |
| red | FF453A | FF3B30 | Heavy CPU, critical, upload, GPU die temp, utilisation 90+, alarm triangle |
| blue | 0A84FF | 007AFF | App memory, performance cores, disk read, charger, GPU power, selected segment, interactive |
| teal | 40C8E0 | 30B0C7 | Cached files, efficiency cores, GPU memory, Media category |
| purple | BF5AF2 | AF52DE | Wired memory, ANE time, AI and ML category, per-process lines in Analytics |
| pink | FF375F | FF2D55 | ANE power (`GPUView.swift:838,999`) |
| indigo-like | 5E5CE6 | 5856D6 | Swap line [01] (exact token not confirmed) |
| quaternary label | system | system | "Free" segment (`PressureStyle.swift:30`) |

Per-metric palette (src unless marked)
- CPU: total line green; level bands under 60 percent green, 60-85 orange, 85+ red (`CPUStyle.swift:12-24`); clusters P blue, E teal, unknown grey (`:68-79`); cluster symbols `cpu`, `gauge.with.dots.needle.67percent`, `flame.fill`.
- Memory pressure: green / orange / red for normal / warning / critical; symbols `checkmark.circle.fill`, `exclamationmark.triangle.fill`, `flame.fill` (`PressureStyle.swift:7-18`). Index rules at 34 and 67.
- Memory composition: wired purple, app blue, compressed orange, cached teal, free quaternary (`PressureStyle.swift:26-31`). Cards: Pressure orange, Free green, App blue, Compressed orange, Cached teal, Swap purple/indigo [01].
- GPU: util green by level, power blue, memory teal, awake orange, ANE time purple, ANE power pink; bandwidth Total teal, Reads blue, Writes orange (`GPUView.swift:804-838,935-999`).
- Network: down green, up red (`NetworkStyle.swift`).
- Disk: read blue, write orange, IOPS white-grey; utilisation grey/orange/red thresholds 60/90 (`DiskStyle.swift:7-14`).
- Capacity slots (light/dark): blue 2A78D6/3987E5, orange EB6834/D95926, aqua 1BAF7A/199E70, yellow (purgeable) EDA100/C98500, magenta (other volumes) E87BA4/D55181; free is `primary` at 0.10 opacity (`DiskStyle.swift:21-39`).
- Temperature: CPU die orange, GPU die red [01]; Hardware sensor tiles run green to yellow-green by temperature [08]; Disk Map and Energy see above. Thresholds not read.
- Energy: charger blue, battery green, consumers orange (`BatteryStyle.swift:53-57`); health value orange when under about 90 percent [09].
- Usage Timeline: blue / green / orange lanes [12].

Surfaces (src): card fill `.quaternary.opacity(0.35)` (metric cards 0.32, hover 0.5), border `Color.primary.opacity(0.08)` 0.5 pt, hover border `tint.opacity(0.45)` (`DashboardView.swift:1213-1220`, `MetricCard.swift:301-311`). Window background in [01] is a blue-grey (about #2B2D3A) because it follows the macOS window material; in [07],[10],[11] it is neutral near-black (about #1E1E20). Treat as `windowBackground` and pick one flat dark value; do not hard-code the tint.

### 3.2 Typography (src, SwiftUI text styles; macOS sizes)
SF Pro. Mapping on macOS: `.headline` 13 semibold, `.subheadline` 11, `.callout` 12, `.caption` 10, `.caption2` 10 (rendered 9-10), `.title3` 15, `.title2` 17 (Apple).
- Panel title: `.headline` (`DashboardView.swift:1183-1185`). Panel icon `.subheadline` secondary.
- Card label: `.caption2` semibold, uppercase, tracking 0.6, secondary (`MetricCard.swift:234-238`). Same style for "HISTORY".
- Card value: `.title3` monospacedDigit semibold, primary, min scale 0.8 (`:253-255`).
- Card detail: `.caption2` secondary. Captions and explanations: `.caption` secondary.
- Table numbers: monospaced digits, right aligned.
- Detail sheet value: `.title3.monospacedDigit` (`MetricCard.swift:617`). Explanation text `.callout`.
- Chart axis labels: about 9-10 pt, secondary (`MetricCard.swift:379` uses system 9).
- Paths and ids: monospaced (`[12]` "App scope").

### 3.3 Spacing and radii (src)
- Page padding 20; column and panel gap 16 (`DashboardView.swift:66`, `PanelLayout.swift:21`). Rail width 300.
- Panel: padding 16, inner VStack spacing 14, header HStack spacing 7 (`DashboardView.swift:1176-1210`).
- Metric card: vertical 11, horizontal 13, label dot 6 pt, label row spacing 5 (`MetricCard.swift:229-299`).
- Sheet padding 24 (`MetricCard.swift:601`). Expand button hit area 24 x 24.
- Radii: panels 12 (continuous), metric cards 10, detail sub-panels 8, menu bar strip 6, chart legend swatches 3 pt tall x 14 wide, capsules for progress tracks (7 pt) (`MetricCard.swift:416`).
- Spacing scale in use: 2, 4, 5, 7, 8, 11, 13, 14, 16, 20, 24. No formal tokens exist; the app uses ad-hoc literals.

### 3.4 Charts (src)
Two renderers: AppKit layer/Canvas strips (`Charts/TrendSurface.swift`, `SparklineSurface.swift`, `LiveSurfaceView.swift`) and SwiftUI `Canvas` (`Charts/TrendChart.swift`). Policy in `docs/dashboard-chart-standard.md` (supersedes `docs/chart-rules.md`).
- Line: monotone cubic (no overshoot, `Charts/MonotoneCurve.swift`), about 120 points across the plot regardless of range, reduced at draw time. Width 2 for trends (`TrendChart.swift:18`), 1.5 for sparklines (`Sparkline.swift:30`), 1.8 for metric detail lines (`MetricChart.swift:128`); round caps and joins.
- Mean line plus translucent min-max "Recorded range" band in the same hue (README, docs); legend "Average (15 s)". Missing data stays a gap, never interpolated.
- Optional area fill: vertical gradient from tint alpha 0.42 at the line to 0.04 at the baseline (`TrendSurface.swift:1411-1415`).
- Reduction per metric: mean for rates/utilisation, max for temperature and fan, last for states and counters (`docs/chart-rules.md`).
- Grid: horizontal lines `secondary` 0.12-0.18, 0.5 pt; vertical time ticks similar; plot border secondary 0.22, 0.5 pt; threshold rules dashed `[4,3]`, width 1, rule colour at 0.4 with a small coloured label at the left (`TrendChart.swift:459-495`).
- Axes: y axis on the left (inside a 56-60 pt gutter) for dashboard and Explorer, right side for per-process analytics [11]. Y ticks: 4-6 evenly spaced labels, bytes formatted (`0 bytes`, `95.4 MB`), percent (`0%`), temperatures with unit. X axis: clock labels HH:mm at even intervals (13:10, 13:15); per-process charts use relative "30m 20m 10m now" [02]. Axis fits the data with a minimum span; true percentages keep 0-100 (`docs/chart-rules.md` rule 5).
- Sparklines on cards: 57 pt high strip, an end-point dot, "Axis max N" caption when the scale is fixed (`MetricCard.swift:158,285`).
- Stacked bar for composition: single rounded bar with segments (memory), 7 pt capsule for gauges, bar-per-core for CPU, dot-grid for SoC.
- Hover: shared vertical cursor with a tooltip showing time and value (`Charts/ChartInteraction.swift`, `ExplorerCursor.swift`). Cmd+scroll zoom (`ChartZoomState.swift`).
- Treemap (`TreemapSurfaceView.swift`) and energy flow (`EnergyFlowView.swift`) are custom canvases.

### 3.5 Icons (SF Symbols, src)
Tabs: `gauge.with.dots.needle.50percent` (Dashboard), `list.bullet.rectangle` (Processes), `display` (GPU), `bolt.fill` (Energy), `network` (Network), `internaldrive` (Disk), `macbook` (Hardware), `waveform.path.ecg.rectangle` (Explorer), `lightbulb` (Insights) (`ContentView.swift:101-150`). Others: `timer` (refresh), `info.circle`, `arrow.up.left.and.arrow.down.right` (expand), `exclamationmark.triangle.fill`, `checkmark.circle.fill`, `flame.fill`, `memorychip`, `cpu`, `thermometer.medium`, `arrow.down`, `arrow.up`, `battery.25/.50/.100` (`BatteryStyle.swift:40-42`). App icon `MacPerformanceMonitorAppIcon.png`; menu bar template `MacPerformanceMonitorMenuBarIcon.png`.

### 3.6 Light and dark
Automatic via system colours and `NSColor` dynamic providers; no in-app theme switch found. Hairlines and fills use `primary`/`secondary` at low alpha so they invert correctly. Screenshots are dark except Ask [14] (light). Reduced motion respected (`APP/Util/ReducedMotion.swift`). Temperature unit follows system but is overridable in Settings, and pages rebuild on change (`ContentView.swift:82-85`).

## 4. Behaviour

### 4.1 History ranges
- Dashboard, GPU, Energy, Processes, Insights: 5 min, 30 min, 1 hr, 6 hr, 24 hr, 7 day (`HistoryQuery.swift:8-28`); default 30 min, remembered per view. Disk 1h/6h/24h/7d. Analytics 2m/1h/6h/24h/7d. Explorer: span dropdown plus go-to-time and zoom. Insights top consumers 1h/6h/24h/7d. Health and Cycles up to 30d / 90d / 6mo / 1y / all.
- Storage (src `CORE/Persistence/Retention.swift:20-24`): raw tier 2 h, 1-minute tier 7 d, 1-hour tier 90 d by default; the user can set the high-res frequency and age, the standard-res frequency and age, and a database size cap of 100 MB to 5 GB (Settings `:569-640`). Queries pick the tier by range; long ranges top up from finer tiers so the line reaches "now".
- Long-range rows carry peaks (`SystemHistoryPoint.peaks`), so the band reaches spikes even from rollups.

### 4.2 Alerts engine (`CORE/Analysis/AlertEngine.swift`, `docs/adaptive-alerts.md`)
Kinds: criticalPressure, swap, processCeiling, leak, highCPU, highGPU, thermalThrottle, sustainedProcessCPU, plus accessory battery (`AlertEngine.swift:134-143`). Defaults (`:6-103`): critical pressure on, memory growth (leak) on, sustained process CPU on, swap off, ceiling off (8 GiB when enabled), high CPU off (85 percent), high GPU off (85 percent), thermal off, accessory battery off (20 percent, range 5-50 step 5).
- Incidents with phases watching, active, recovering, resolved, unknown; stable ids per condition (`AlertEngine.swift:156-170`). Process identity is PID plus start time.
- Swap: growth floor max(1 GiB, 3% RAM), 5 and 15 minute windows, last 2 minutes must still grow, confirmation 1 min; fast guard max(2 GiB, 10% RAM) in about 2 min with 30 s confirmation; paging test needs swap-in and swap-out at least 8 MiB/s each with elevated pressure for 2 min.
- Process growth: slope 8 KiB/s and 32 MiB floor, at least 20 min of data, 5 percent relative growth, warning at max(512 MiB, 3% RAM); fast guard max(1 GiB, 8% RAM) in about 2 min. History kept 1 h in 30 s buckets for up to 4,096 processes.
- Busy for hours: program (by executable) averaging at least 80 percent of a core over 1 h, busy three quarters of it; spell starts at 25 percent, ends after 10 quiet min; kernel_task excluded; apps become a warning only after 3 h; at most 256 tracked.
- Notification policy: 5 min refire cooldown delays rather than drops; at least 60 s between batches; critical upgrades bypass; related memory alerts share one notice; only critical requests sound; compare against last notice values (`AlertEngine.swift:218`).
- UI: red badge count in the menu bar panel; quiet "Observations" list for modest or settling items; alert title opens Explorer at the evidence window; bell button snoozes one hour; "Observe growth without notifications" option. Notifications through `UNUserNotificationCenter` (`APP/Alerts/AlertNotification.swift`). Incident evidence persisted (`AlertIncidentStore.swift`).
- Settings sections per alert (`SettingsView.swift:326-440`).

### 4.3 Per-process history
Raw and minute-tier per-process rows for every process while recording is on (`CORE/Persistence/ProcessHistory.swift`); fields: memory footprint, CPU percent of one core from CPU-time deltas, threads, file descriptors, disk read/write, network (opt-in per-app tracking), Rosetta flag. Inspector shows 5m-7d charts; earlier non-overlapping runs of the same program can be overlaid; Usage Timeline lanes; Explorer compares up to 8 including exited processes; trace export `.mpmtrace` (`CORE/Export/ProcessTraceCodec.swift`).

### 4.4 Hardware inventory
Snapshot, not history. Readers: `system_profiler` parsing, IOKit/SMC sensor reader, native readers (`CORE/Hardware/*`). Searchable outline, export to report. Sensors panel is the only live part (5 min charts at refresh cycle).

### 4.5 Energy
See 2.4. Data: `CORE/Models/BatterySample.swift`, `Persistence/BatteryHistory.swift`, `Analysis/BatteryRuntimeEstimator.swift`, `EnergyImpact.swift` (per-process impact). Accessory batteries read at most once a minute (`AccessoryBatteryModel.swift`).

### 4.6 CLI and agent access
`mpm` binary inside the app bundle: `mpm mcp` serves a read-only MCP server with five tools (data guide, SQL query, judged summaries, process finder, open-in-app links); `mpm help` for the CLI; `mpm coverage` shows history depth (`docs/ai-agents.md`, `CORE/Agent/*`). The Ask window can copy a hand-off prompt. Sparkle auto-update, Homebrew cask.

### 4.7 Settings (`APP/Views/SettingsView.swift`, sections in order)
General (open at login, start minimised), language, temperature unit (system/C/F), record history toggle, refresh interval, Ask toggle, menu bar (show item, presentation Focus/Strip, focused read-out, per-metric checkboxes), Dock pinning, Alerts (one toggle plus threshold per rule), per-app network tracking, "Show every process" (privileged helper, Full Coverage), Full Disk Access for Disk Map, high-res and standard-res logging frequency and age, database size slider with projected size and warnings, updates. Refresh interval choices: 250 ms, 500 ms, 1, 2, 5, 10 (default), 30, 60, 300 s; label "250ms", "5s", "5m" (`APP/ViewModels/SamplerModel.swift:821-831`). Subsecond values only speed up light system charts. Languages en, zh-Hans, de, fr.

## 5. Mapping to our data

Our fields from `/home/mahfuz/Projects/sysman/crates/schema/src/*.rs`. History: raw 2 s for 48 h, 1-min rollups 90 d, 15-min rollups 2 y. Their tiers are shorter at the fine end (2 h raw) and they have no 2 y tier, so our retention exceeds theirs. Their 5 min and 30 min ranges work from raw 2 s data (150/900 points); 7 days uses our 1-min tier; the Dashboard standard of about 120 points per plot fits.

| Screen / widget | Status | Our fields or substitute |
| --- | --- | --- |
| Dashboard: Pressure card and chart (0-100 index, 34/67 rules) | partial | Linux has PSI, not a kernel 3-state level. `pressure` stall `memory.some/full avg10/avg60`. Derive index from `memory.some.avg10` (and `full`) plus swap rates; define bands ourselves. No kernel level. |
| Free / App / Compressed / Cached / Swap cards | partial | Free: `memory.free_bytes` (or `available_bytes`). App: `anon_bytes`. Cached: `cached_bytes`+`buffers_bytes` or `cache_bytes`. Swap: `swap_used_bytes`, `swap_total_bytes`. Compressed: only zram: `zram[*].compressed_bytes`/`orig_bytes`, not applicable if no zram. No "wired"; closest `kernel_stack_bytes + page_tables_bytes + slab_bytes`. |
| Memory composition bar | partial | Wired to kernel (slab, stacks, page tables), App to anon, Compressed to zram mem_used, Cached to cache, Free to free_bytes. Fields are Option; may be missing. |
| Processor chart, LIVE TOTAL | available | `cpu.total_pct`, `breakdown_pct.{user,system,iowait,steal,...}` |
| LIVE PERFORMANCE / EFFICIENCY | partial | `cpu.core_pct` plus `topology.core_types` (hybrid Intel only). On non-hybrid or AMD show per-CCD from `topology.l3_groups` instead. |
| LIVE LOAD (1 MIN), load avg 5/15 | available | `system.load[0..2]` |
| CPU cores bars and tooltip | available | `cpu.core_pct` (per logical CPU), `core_mhz`; cluster badge from `core_types`/`l3_groups` |
| Uptime, boot date | available | `system.uptime_s`, `system.boot_time` |
| Header brand, cores | available | `cpu` meta `model`, `topology.cores/threads`, `memory.total_bytes` |
| Network panel, down/up | available | `net.interfaces[*].rx_bps/tx_bps`, totals `rx_bytes_total/tx_bytes_total` (session total needs delta from first sample) |
| Disk throughput, IOPS, latency, utilisation | available | `disk[dev].read_bps, write_bps, read_iops, write_iops, read_await_ms, write_await_ms, busy_pct, queue_depth` |
| Free space chart | available | `fs[mount].used_bytes/avail_bytes/total_bytes`. Purgeable: none (APFS only). |
| Thermals (CPU, GPU die) | available | `cpu.temp_c`, `cpu.temps_c`, `gpu.devices[*].temp_c`, `hwmon` |
| Top CPU / Top disk in rail | available / partial | `processes.top_cpu`, `top_io` (optional, needs permission) |
| Processes table (full) | available | `processes.table.rows`: `pid, ppid, comm, state, threads, rss_bytes, cpu_pct, read_bps, write_bps, user, fds, cmdline`. Substitute RSS for `phys_footprint` (counts shared pages); no PSS or USS. No "Arch" column; substitute `user` or `state`. Icons need `.desktop` lookup by `comm`/cmdline. Hierarchy from `ppid`. |
| Processes: 576 processes, header cards | available | `processes.count`, `threads`, `states` |
| Process inspector charts (memory, CPU, FDs, disk I/O) | partial | Stored only for top N in `processes`; for other PIDs live-only. Per-process network: no. Per-PID history is limited to what top N storage kept. |
| Process glossary card | not in data | ship own `comm` to description file |
| Force quit | out of collector | action, not data (kill syscall) |
| GPU: util, power, memory, clocks | available (vendor-varying) | `gpu.devices[*].util_pct, power_w, vram_used_bytes/total, clock_mhz, mem_clock_mhz, temp_c, fan_*, throttle_reasons, pstate, pcie_*, encoder_pct/decoder_pct` |
| GPU AWAKE, clock-state residency bars | partial | `idle_pct`, `power_state`, `pstate` give one state, not residency over time; derive from history. |
| GPU bandwidth Reads/Writes/Total | macOS-only | closest `mem_busy_pct` and `mem_clock_mhz` |
| Neural Engine (time, power) | macOS-only, no Linux equivalent | none; drop, or show NPU if a driver ever reports (not in schema). Encoder/decoder `video_busy_pct` is the nearest media-engine stat. |
| GPU By category / AI workloads | partial | `gpu.processes[*]` (`util_pct, mem_bytes, encoder/decoder`); category must be inferred from `comm` list (ollama, llama, python) |
| Energy: charge, health, cycles | available | `power[BAT*].capacity, cycle_count, energy_wh, energy_full_wh` (design capacity not in schema; health needs `energy_full_design`, missing) |
| Energy: power draw, voltage | available | `power[BAT*].power_w, voltage_v`, `cpu.power_w`. Whole-system draw absent on most laptops except battery discharge rate. |
| Estimated runtime | partial | compute from `energy_wh / power_w`; no OS estimate |
| Battery temperature | partial | `hwmon` if exposed |
| Condition, manufacturer | available | `status`, `manufacturer`, `model_name`, `capacity_level` |
| Energy flow diagram, per-process energy | partial | Per-process energy impact absent. Substitute CPU share from `processes.top_cpu`. |
| Accessories (mouse, headset) | available | `power` supplies with `scope == "Device"` |
| Charger rating ("96 W max") | missing | not in schema |
| Network adapters list, IP, MAC, router, DNS | available | `net.interfaces` + `netinfo.interfaces` (`mac`, `addresses`, `mtu`, `state`), `default_routes`, `dns`; Wi-Fi `signal_dbm`, `link_quality`, `link_mbps` |
| Per-app network | macOS opt-in; no Linux data | not collected (needs eBPF or nethogs) |
| Disk health (SMART): life used, spare, hours, errors | partial | `storage.devices`, `storage.btrfs[*].device_errors`; `storage` meta has model, serial, firmware, rotational. No SMART/NVMe wear, temperature (use `hwmon` nvme), power-on hours: missing. |
| Disk Map treemap | not in data | needs separate scanner (walk `fs` mount). `fs` gives only totals; `fs.inodes_*` extra. |
| Hardware inventory | partial | `system` meta `dmi`, `kernel`, `distro`; `cpu` meta; `gpu` `DeviceInfo`; `storage` meta; `netinfo`. Displays, USB, Thunderbolt, audio, cameras, PCI, security: not collected. |
| Hardware Sensors grid | available | `hwmon` (temp, fan, voltage, power) with `HwmonMeta.limits` for crit/max |
| Fans | available | `hwmon` fan kind, `gpu.fan_rpm` |
| Explorer: machine charts | available | all metric modules; 23 machine charts translate mostly 1 to 1 |
| Explorer: process charts | partial | top-N only (see above) |
| Insights: swap climbing, pressure events | available | derive from `memory.swap_used_bytes` and `pressure` history |
| Insights: top consumers over time (avg/peak) | partial | top-N processes in rollups; `processes.groups_cpu/groups_memory` give by-name sums |
| Memory growth / leak | partial | needs per-process memory history beyond top N; `cgroups.top_memory` gives per-service totals |
| Groups | partial | substitute `cgroups` (systemd slices/scopes: `cpu_pct, memory_bytes, pids`) and `processes.groups_*` |
| Alerts: pressure, swap, CPU, GPU, thermal, battery | available | evaluate on collected modules; `memory.oom_kills_total` is an extra Linux signal; notifications via freedesktop (D-Bus) |
| Alerts: process ceiling / sustained CPU | partial | top-N storage limits detection window |
| Usage Timeline (app/media activity) | macOS-only | `knowledgeC` based; running lanes can use process history; no Linux substitute for media/app usage |
| Ask About This Mac | macOS-only (Apple Foundation Models) | substitute: rule-based summary cards from the same data; optional agent via MCP |
| `mpm` CLI/MCP | n/a | could expose SQLite read-only |
| Menu bar item and popover | no equivalent | see section 6 |
| Refresh control 250 ms-5 min | partial | collector is 2 s raw; UI choices 2 s and above only |

## 6. Gaps and judgement calls

1. No global menu bar on Hyprland/Wayland. Closest: a StatusNotifierItem tray icon (no live text), a bar module (Waybar, DMS widget) fed by our data, or a small always-on-top layer-shell overlay window. The strip of RAM/CPU/GPU cells with alarm triangle is a bar widget, not an app feature. Recommend deferring and shipping the main window first.
2. Window chrome: they rely on native unified toolbar with centred segmented tabs and traffic lights. GPUI draws client-side decoration; under Hyprland use a custom titlebar (or none) and a left sidebar or top tab row. Tab count is 10; on a narrow tiling window the centred strip needs to collapse (they measure minimum width from titles, `ContentView.swift:5-30`). Decide early whether to use the same segmented strip or a sidebar.
3. Pressure semantic. macOS gives a discrete kernel level; Linux gives PSI percentages. Index formula and the 34/67 rules cannot be copied; calibrate from `pressure.memory.some.avg10`. Flag as our own design.
4. Memory taxonomy. Wired/Compressed/App/Cached is macOS-specific. Our substitute breakdown will not sum to the total unless built from `total - available` with the fields we have; show an "Other" segment.
5. Apple silicon concepts without Linux equivalents: P/E cluster, ANE, unified memory, GPU clock-state residency, SoC dot-grid tiles, Rosetta, Arch column, purgeable space, APFS container free space, FileVault/Secure Boot rows.
6. Per-process history. They record all processes at 1-5 s; we store top N only. Inspector charts for an arbitrary row will be live-only unless that PID was in top N. Needs a product decision (store more, or show "recorded only for top processes").
7. `phys_footprint` vs RSS. Our RSS overcounts shared memory, so process totals can exceed RAM. Label the column "RSS" or "Memory (RSS)".
8. Appearance cannot be copied exactly. SwiftUI materials, `.quaternary` fills and system colours have no GPUI equivalents; define explicit tokens from the table in 3.1 and 3.3 (card fill about white at 6 to 8 percent over the window colour, 0.5 pt border white at 8 percent, 12 px panel radius, 10 px card radius, 16 px padding). Our window background must be chosen; their dark tone is about #1E1E20 to #2B2D3A.
9. Fonts: SF Pro is unavailable. Use a system sans (Inter or Cantarell) with tabular digits for numbers; the relative scale 9-10 / 11-12 / 13 / 15 / 17 carries over.
10. Light mode: only the Ask window is shown in light. GPUI themes must supply both; dark is the reference.
11. Chart engine: no SwiftUI Charts; they hand-wrote Canvas renderers. In GPUI plan for custom paths (monotone cubic, band, gradient, dashed rules) via the canvas/path API. Their documented rules (about 120 points, mean line plus min-max band, gaps, draw-time reduction) are the part worth copying.
12. Refresh: their default 10 s UI refresh with 5 s floor on process scans is a CPU budget choice; our collector already runs at 2 s, so the UI should follow collector ticks and avoid re-rendering hidden tabs (they unmount inactive tabs; see the memory-growth warning in `ContentView.swift:57-63`).
13. Privilege: their "Full Coverage" helper daemon for all processes and Full Disk Access prompts have no direct Linux analogue; our collector already notes missing `read_bps/user/fds` for other users' processes, so show "not permitted" rather than 0.
14. Ambiguities not resolved: exact temperature colour ramp; Disk Map kind hex values; whether the Explorer grid is 3 columns at all widths (screenshot only); the indigo swap tone. Screenshots [07],[10],[11] predate the GPU/Hardware tabs, and [03],[04],[12],[14] use sample data.
15. Disk Map, Ask and Usage Timeline are the biggest scope risks and the least translatable. Suggest leaving them out of v1.
