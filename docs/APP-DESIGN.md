# sysman app — design

The GPUI app that shows `sysmand`'s data: what it looks like, how it is built,
and what each screen contains.

## 1. Stack

| Piece | Choice | Why |
| --- | --- | --- |
| UI framework | `gpui-pre` + `gpui-pre-platform` from crates.io, renamed to `gpui` / `gpui_platform` | A pinned snapshot of Zed's gpui (`gpui-pre 0.3.7` = zed@1a28cff), and the gpui `gpui-component` builds against |
| Components | `gpui-component` (pinned with `=`) | Virtualised sortable table, segmented tab bar, resizable panes, sidebar, menus, tooltips, client-side title bar, themes |
| Charts, treemap, energy flow, core bars | Custom elements on `canvas` + `PathBuilder` | `gpui-component` charts lack monotone cubic, min–max band, labelled dashed rules, gaps and a shared cursor |
| Fonts | The system sans-serif (UI, tabular digits) and monospace (paths, ids), as fontconfig resolves them | Matches the desktop; nothing bundled |
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
  right, the refresh control and a Settings button. Under Hyprland tiling, the
  title bar still drags floating windows and shows no shadow or inset when tiled.
- Settings opens as a modal dialog over the window (collector and helper
  services, refresh interval), never as a tab.
- Side panes (Explorer, Hardware, Groups, Logs, Disk Map rail, the process inspector)
  are resizable by dragging their edge and collapsible; widths and collapsed
  state are remembered.
- Tabs and Lucide icons: Dashboard `gauge`, Processes `list`, GPU `monitor`,
  Energy `zap`, Network `network`, Disk `hard-drive`, Hardware `cpu`, Explorer
  `activity`, Insights `lightbulb`, Logs `file-text`, Groups `layers`. Disk Map
  is a sub-tab of Disk.
- When the window is narrower than the strip, labels collapse to icons with
  tooltips.
- Default 1280×720, minimum 860×520.

## 4. Visual tokens

Dark and light, with System following the desktop; every token lives in
`ui/theme.rs`. The table gives the dark values.

| Token | Dark |
| --- | --- |
| window | `#21232F` (blue-grey, not black) |
| panel fill | white 5% over window |
| card fill / hover | white 5% / 8% |
| hairline | white 8%, 0.5 px |
| text / secondary / tertiary | white 92% / 60% / 38% |
| green orange red blue teal purple pink indigo | `30D158 FF9F0A FF453A 0A84FF 40C8E0 BF5AF2 FF375F 5E5CE6` |

Selected segments of every segmented control and range picker are filled with
the accent blue.

Metric colours: CPU green with 60/85 bands, memory pressure orange, app memory
blue, cache teal, kernel purple, swap indigo, download green, upload red, disk
read blue, write orange, CPU temperature orange, GPU temperature red.

Typography (the system sans-serif, tabular digits for every number): panel title 13
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

Shared widgets (`ui/widgets/`): `panel` (titled, with an expand arrow) and
`section` / `section_with` (no arrow, optional subtitle); `card`; `segmented`;
`range_picker`; `legend`; `text` (`caption`, `footnote`, `key_value`, `facts`,
`proportion`); `helper_hint` (lock glyph, one line, "Enable…" opens Settings);
`app_icon`; `sidebar`; `detail_sheet`.

Detail sheet: every chart panel's expand arrow, and every metric card, opens a
modal with the chart at 260 px and its own range picker, window statistics per
line (average, minimum, maximum, latest, stored sample count), "What this
means" and "How it is calculated". Panels without a chart have no arrow.

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
  GPU `temp_c` red), Top CPU, Top disk (live), Recent events.
- Recent events: up to 5 rows from the `events` table for the Dashboard's
  range, most severe first (crash, boot ended without shutdown, OOM kill,
  restart loop, failed unit, packages), each a glyph, a one-line summary and
  its time; a row opens Logs at that entry. With none, one quiet line
  ("Nothing notable in the last 24 hr"). It reads only `events`, never the
  journal.
- Clicking a card, or a chart panel's arrow, opens its detail sheet.

### 6.2 Processes
- Header cards: CPU USAGE (user/sys split, peak), CORES, LOAD AVERAGE, CPU
  TEMPERATURE; each opens its detail sheet.
- Table, from socket `processes` every refresh (the full table). Columns,
  chosen from a Columns menu and remembered: PID, Process (icon from the `.desktop`
  file matched by executable, else a generic glyph), Command, Path, Memory
  (RSS, shared pages counted in full), Mem % (two decimals below 1%, "<0.01%"
  below that), Virtual, CPU % (of one core), CPU time, Threads, FDs, Read/s,
  Write/s, Net ↓, Net ↑, User, State, Nice, Priority, Started, PPID.
  Default sort Memory descending; the whole header cell sorts. Search filter;
  Hierarchy toggle (tree by ppid, flattened into rows); selection kept by
  pid + start time across re-sorts.
- Inspector (resizable, collapsible), top to bottom: name and PID; **Why it's
  running** (below); details (user, start time, state, command line);
  **Listening on** (below); range picker; charts Memory, CPU, File
  descriptors, Disk I/O from `proc_points` / `proc_rollup_1m`, sharing a
  cursor. Earlier runs of the same command (same name and command line,
  started before this one) are summed and drawn as dashed lines on the
  Memory, CPU and FD charts.
- **Why it's running**: one plain sentence naming the cause of the selected
  running process, the chain that leads to it, and what ending it would do.
  Worked out only for the selected process, once per selection (pid + start
  time), never per row and never stored. Facts only: every cause names its
  evidence, and what cannot be known is said ("sysman can't tell why: …").
  - *Cause* from `/proc/<pid>/cgroup` (any user may read it) and the parent
    chain, classified by a pure function in `data/why.rs`. The cgroup comes
    first because an orphan is re-parented to PID 1 or a subreaper, so the
    parent chain alone misleads. Kinds and their sentences:

    | Kind | Evidence | Sentence |
    | --- | --- | --- |
    | System or user service | `….service` in the cgroup; unit `UnitFileState`, `WantedBy` | "System service *OpenSSH Daemon* (sshd.service), enabled at boot." |
    | Socket-activated | unit `TriggeredBy` a `.socket` | "Started when something connected to port 631 (cups.socket)." |
    | Timer | `TriggeredBy` a `.timer`; the timer's `LastTriggerUSec`, `NextElapseUSecRealtime` | "Started by the timer man-db.timer at 03:00; next run 03:00 tomorrow." |
    | D-Bus activated | unit `BusName` and nothing wants it | "Started on demand for org.freedesktop.UDisks2." |
    | Login autostart | `app-…@autostart.service`, unit `SourcePath` | "Starts when you log in, from ~/.config/autostart/x.desktop." |
    | App | `app-…<desktop id>….scope`, name from the `.desktop` file | "You opened *Firefox* at 09:12." |
    | Terminal | the parent chain reaches a terminal or shell | "Run in a terminal: kitty → zsh → cargo." |
    | Orphan | parent is PID 1 or a subreaper, cgroup not its own unit, and its session leader (`stat` field 6) no longer runs | "Started by a terminal or script that has since exited." |
    | Login session | `session-N.scope`; logind `Type`, `Remote`, `RemoteHost` | "Part of mahfuz's SSH login from 192.168.1.5." |
    | Container | `docker-<id>`, `libpod-<id>`, `kubepods`, `machine.slice` | "Runs in Docker container 3f2a9c…" (name when readable) |
    | Flatpak or Snap | `app-flatpak-<id>-…`, `snap.<name>.…` | "Flatpak app com.discordapp.Discord." |
    | Cron | the parent chain passes `crond` or `cron` | "Started by cron." |
    | Kernel | pid 2 or ppid 2 | "Part of the Linux kernel; it can't be ended." |
    | Unknown | none match | "sysman can't tell why: <reason>." |

    cgroup v1 hybrids use the `name=systemd` line; without systemd the
    parent chain alone gives the answer. Terminal multiplexers (tmux,
    screen) are named only on an exact `comm` match. One fact line is added
    under the sentence when it holds: the program file was replaced or
    removed since it started (`/proc/<pid>/exe` ends in " (deleted)"), "Running
    an older copy of this program; restart it to use the installed one."
  - *Details* for the selected process only: unit properties over D-Bus
    (system bus for system units, session bus for user units), the logind
    session, the `.desktop` name, the container name (podman's
    `containers.json`, Docker's API when its socket is reachable). Two
    passes run side by side: if D-Bus has not answered within 150 ms, the
    sentence from the cgroup shows with a "Looking up details…" line;
    details fill in when D-Bus answers, and a line says so if it does not.
  - *Layout*: the sentence; the chain, at most four lines, cause on top and
    the process last (`multi-user.target` → `sshd.service · restarted 0
    times` → `sshd 812`), ellipsised with the full text on hover; **If you
    end it** ("systemd starts it again", "it comes back at the next login",
    "it starts again on the next connection"), left out when it cannot be
    known and once the process has exited; actions; **How sysman knows**
    (cgroup path, unit file, `WantedBy`/`TriggeredBy`), folded by default and
    remembered. The restart count is the unit's `NRestarts`. "If you end it"
    follows the unit's `Restart=` and whether the process is its `MainPID`:
    ending a process that is not the main one restarts nothing, and SIGTERM
    counts as a clean exit to `on-failure` while SIGKILL does not.
  - *Actions*: Stop, Restart and Disable for a unit (system units through
    polkit, user units only when they are this user's), each confirmed, through
    the systemctl runner Settings uses; "Logs for this unit" opens Logs with
    `unit:<name>`; for containers the command to copy, never run.
- **Listening on**: the selected process's listening TCP sockets and bound
  UDP sockets, each "this machine only" (loopback) or, by its bound address,
  "reachable from the network" (TCP) or "can receive from the network" (UDP,
  never called a service: clients' DNS and QUIC sockets are bound too);
  sockets that read the same are listed once. From the daemon's `listeners`
  socket request (DESIGN.md, "Privileged helper"), once per selection. For
  another user's process with no sockets matched while `holders_error` is set,
  one line gives the reason with the "Enable…" hint. Hidden when it has none.
- The search field also takes exactly `port:<n>`: the table lists every
  process, `listeners` is asked once typing pauses, and one line under the
  field names the holder, which is selected (a process other than PID 1 or a
  user `systemd`); else the socket unit holding it and the service it starts
  on the next connection, the owner when the holder cannot be read, the
  kernel, or "Nothing is listening on port N."
- Context menu: Copy name/PID/command line; End process (SIGTERM) and Force end
  (SIGKILL), each confirmed, the confirmation warning when the process will
  come back; disabled for other users' processes.
- Values the collector may not read (other users' I/O and FDs) show "–" with a
  "not permitted" tooltip, never 0.

### 6.3 GPU
- Device picker when there is more than one GPU.
- Cards: GPU % (green by level), POWER, VRAM, TEMPERATURE, CLOCK, VIDEO
  (encode/decode or `video_busy_pct`).
- Panels: utilisation (green area), VRAM used, power with the power-limit rule.
- Rail: Device (name, driver, backend, PCIe current/max, power limit, fan,
  power state, clock states as residency bars of the performance states seen
  since the screen opened, throttle reasons as capsules); Processes (name,
  util, VRAM); By
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
Header "Primary: <iface> · Router <gateway>"; big DOWNLOAD/UPLOAD; range
picker; throughput chart; session totals; Per-app traffic (needs the helper's
eBPF counters, else a helper hint); Adapters (glyph, name, Primary/Connected/No
address capsules, IPv4/IPv6, MAC, rates, Wi-Fi signal; the chevron expands MTU,
link speed, packets, errors, drops, state and the adapter's own chart, whose
arrow opens its detail sheet); Configuration (host name, primary interface,
default routes, DNS); rail with TCP and Sockets charts.

### 6.6 Disk and Disk Map
- Disk: device picker; cards READ, WRITE, IOPS, FREE SPACE; panels Throughput,
  Operations and latency (await, busy % with 60/90 tint, queue depth); Devices
  list (model, serial, firmware, state). Rail: Health (NVMe temperature from
  hwmon, controller state, btrfs error counters and missing devices; SMART
  health through the privileged helper), Free space per filesystem with the
  used-space chart, Top disk processes (other users' I/O through the helper).
- Overview / Disk Map view switch in the header.
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
visible range. A process picked from the search, running or exited, shows one
**Why it ran** line under its name (below).

### Why it ran (recorded processes)

`procs` keeps each instance's cgroup path and parent pid, written once when
`sysmand` first records it (DESIGN.md). For a process that is no longer
running, Explorer and the Logs inspector show one sentence from that record,
classified by the same function as the live inspector, with the parent chain
rebuilt from recorded `procs` rows (the parent instance that ran at the
child's start). Unit details (trigger, restart policy) are not recorded: when
the unit still exists they are looked up now and the sentence says "as of
now"; otherwise the sentence names only the unit, app, container or session.
Rows recorded before this existed say "Not recorded (sysman was older then)".
Nothing here runs in the daemon or per tick.

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

### 6.11 Logs
One place to read, filter and diagnose what the system logged: the systemd
journal (kernel, hardware, services, apps, scheduled jobs, sign-ins, boot and
power, crashes) and the package manager's history, on one timeline, next to
what sysman recorded at the same moment. Reading and the index live in
`crates/journal` (DESIGN.md, "Logs and events"); the screen is
`screens/logs/`, its index `data/logs.rs`.

**Layout** (left to right): sources sidebar, main column, inspector.

- *Sources sidebar* (resizable, collapsible): categories with entry counts for
  the current scope (All, Kernel, Hardware, Services, Apps, Scheduled,
  Sign-ins & privileges, Boot & power, Crashes, Packages); a **boot timeline**
  (current boot on top; each boot with start → end, duration, suspended spans,
  "Clean shutdown" or "Ended without shutdown" in red, a one-line tally of
  crashes, failed units and OOM kills; the off time between boots drawn as a
  gap, "Ended without shutdown · next boot 93 s later"); saved filters (a name plus a query
  string, stored in `ui.toml`).
- *Main column*, top to bottom:
  1. Toolbar: search field (free text plus tokens, below), priority
     segmented control (All, Notice+, Warning+, Errors), range picker
     (5 min … 7 day, "This boot", or the boot picked in the sidebar), Follow
     toggle (live tail, on only when the scope reaches now).
  2. **Worth a look**: one row of finding cards for the scope (below), most
     severe first, at most two rows; each card is a title, one line of
     evidence and a count. Clicking one narrows the list to its entries,
     grouped under the finding, with "what sysman recorded" charts above the
     group. Collapsible; remembered.
  3. **Timeline strip**: entries per bucket stacked by priority, the same
     buckets as the list's scope; drag across it to narrow the range,
     double-click to clear. Its expand arrow opens the **lanes view** in
     place: one lane per category with event marks, CPU use and CPU
     temperature from history drawn behind the lanes, boot starts and ends as
     rules (boots that ended without shutdown in red). Marks are at least 3 px wide and the
     nearest mark within 6 px is the one hovered.
  4. **Log list**: virtualised, one dense row per entry (time, priority mark,
     source, message in monospace, ellipsised). Identical consecutive
     messages from the same source fold into one row with a "×N" pill. A
     boot boundary is a full-width divider row; a boot that ended without
     shutdown says so on its divider in red.
- *Inspector* (resizable, collapsible, shown only for a selection, like the
  process inspector): priority, source and time; the full message
  (multi-line, selectable); the finding the entry belongs to, if any;
  **What sysman recorded** — CPU, memory and CPU temperature around the entry
  from history (±10 min, the entry's time as a rule), plus the process's own
  memory history when the entry names a pid sysman recorded (crashes, OOM
  victims) and its **Why it ran** line (below); "Open in Explorer at HH:MM"; **Around this entry** — the 6 entries
  before and after it with no filter applied; every journal field as a
  key/value list. Actions: Copy (entry as text), Copy fields (JSON), Show
  backtrace (crashes: the stack trace from the coredump's MESSAGE, else
  `coredumpctl info <pid>` output, in a sheet), Restart unit (failed units,
  through the same systemctl runner Settings uses, confirmed first).

**Query tokens** (also what saved filters store): `unit:`, `prio:` (a level
name or number, meaning that level and more severe), `boot:` (`0`, `-1`, …),
`source:` (syslog identifier), `cat:` (a category key), `pid:`, `since:` /
`until:` (`1h`, `2026-10-15 20:00`), free text matched case-insensitively
against the message. Tokens AND together; a token's repeated values OR.

**Findings** (rules in `crates/journal`, not guesses): crash (coredump entry;
"Second Hyprland crash this boot" when repeated), OOM kill (kernel or
systemd-oomd), failed unit, restart loop (≥3 restarts of a unit within
5 min), boot ended without shutdown (no logind shutdown message and no PID 1
"multi-user.target stopped" in the boot's final stretch; "confirmed" when
the next boot finds the journal uncleanly closed or the CPU reports a reset
reason, "ended while suspended" when the last event was a suspend; never
judged from the user's own journal alone, nor for boots whose start was
vacuumed; never called a power loss unless a reset reason says so; evidence:
sysman's last readings before the end), thermal throttling (kernel throttle
lines, counted; AMD CPUs log none), hardware errors (warning or worse: MCE,
PCIe AER, EDAC, GPU faults such as NVIDIA Xid, amdgpu ring timeouts and
i915/xe hangs, NVMe, ATA and block I/O errors, USB disconnect storms;
corrected errors rank below uncorrected ones), crashes also from kernel
segfault lines where systemd-coredump isn't installed, repeated
failed sign-ins, package changes (a transaction's upgraded packages; flagged
when a finding follows within the same boot).

**Scale.** A scan builds a compact index of the scope (time, priority,
category bits, interned source; about 16 bytes an entry) on the background
executor, showing progress; filters, counts and the strip are computed from
the index, messages are read only for visible rows (by seeking), and free-text
search streams matches as it scans. The default scope is This boot (about
100 k entries here); 7 days is about 1 M. A new scope cancels the running
scan.

**Access.** Without read access to the system journal (not in
`systemd-journal`, `adm` or, on Arch, `wheel`), a banner says only this user's
entries are shown and gives `sudo usermod -aG systemd-journal $USER`. Without
libsystemd the page falls back to `journalctl`; without either, it says so.

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
4. Energy, Hardware. Done.
5. Explorer, Insights, Groups. Done.
6. Disk Map. Done.
7. Logs and event markers on charts. Done.

Each milestone ends with the screen running against the real daemon and
checked by screenshot.
