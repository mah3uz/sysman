# Changelog

## Unreleased

## 0.3.0 - 2026-10-23

- **Why it's running:** select a process and the inspector says, in one sentence, why it exists, with the evidence
  behind it.
  - **Causes:** a system or user service enabled at boot, a socket, timer or D-Bus activation, a login autostart entry,
    an app you opened, a command run in a terminal, a process whose parent has exited, an SSH or other login session,
    a container (Docker, Podman, Kubernetes), Flatpak and Snap apps, cron, and the kernel. When sysman can't tell, it
    says why rather than guessing.
  - **The chain** from the cause down to the process, the unit's restart count, and the last and next run of a timer.
  - **If you end it:** whether systemd starts it again, after End or only after Force end, or whether it comes back on
    the next connection or login. The End process confirmation repeats this.
  - **Actions:** Stop, Restart and Disable the unit, each confirmed, and Logs for this unit. They are never offered for
    the unit that holds your desktop session.
  - **How sysman knows:** the cgroup, unit file and dependencies behind the answer.
  - A process still running an older copy of a program that an update has since replaced says so.
- **Listening on:** the selected process's open ports, each marked reachable from the network or from this machine
  only. Search `port:631` to find which process holds a port, or which socket unit is waiting on it.
- **Why it ran:** Explorer and the Logs inspector say why a recorded process ran, including processes that have
  exited. `sysmand` records each process's cgroup and parent once, when it first sees it; existing history is upgraded
  in place, and processes recorded before this say so.
- **Processes:** PID is the first column.
- `sysman-helper` can name which of another user's processes holds a listening port. It reveals only listening
  sockets, never connections.
- `sysmand` answers a new `listeners` request on its socket.

## 0.2.0 - 2026-10-20

- **Logs**, a new tab: the systemd journal and the package manager's history on one timeline.
  - **Sources:** kernel, hardware, services, apps, scheduled jobs (cron and timers), sign-ins and privileges, boot and
    power, crashes, and package changes (pacman, apt and dpkg).
  - **Finding entries:** search with tokens such as `unit:`, `prio:` and `boot:`, filter by priority and range, follow
    live, and save filters.
  - **Worth a look** cards come from fixed rules:
    - crashes, folded per program;
    - OOM kills;
    - failed units and restart loops;
    - boots that ended without a shutdown, with the CPU's reset reason when AMD processors report one, and sysman's
      last readings before the end;
    - GPU faults (NVIDIA Xid, amdgpu, i915 and xe) and disk and NVMe errors;
    - thermal throttling and repeated failed sign-ins.
  - **Boot timeline:** shows how each boot ended and how long the machine was off.
  - **Inspector:** the entry, the entries around it, every field, and what sysman recorded at that moment (CPU, memory,
    temperature and a crashed process's memory), with a link to Explorer at that time.
  - **Lanes view:** draws each category against CPU use and temperature.
  - **Actions:** Copy, Show backtrace, and Restart unit for failed units. Restart unit asks first, and is never offered
    for session or display-manager units.
- **Journal access:** without read access to the system journal (membership in `systemd-journal`, `adm`, or `wheel` on
  Arch), Logs shows only your own entries and says so, and it never guesses how boots ended.
- **Crashes without systemd-coredump:** where it isn't installed (the default on Debian and Ubuntu), crashes are found
  from the kernel's segfault lines.
- `sysmand` records notable events into history: crashes, OOM kills, failed units, restart loops, boot starts and ends,
  suspend and resume, and package transactions. Only short summaries are stored, never log text, and they are kept for
  `retention.events_days` (default 90, also in Settings → Collector).
- **Event markers:** Dashboard and Explorer charts mark these events, with a tooltip and a click through to Logs. The
  Dashboard rail has a Recent events card.
- `history.db` and the crash log are now readable only by their owner.
- **Settings:** General and Display are one tab, and the dialog's corners are rounded.
- **Processes:**
  - the inspector has its own time range, so changing it no longer changes the cards above the table;
  - the details button reflects whether the panel is open;
  - a selected row is tinted;
  - the table has a left inset.
- **Right-hand columns:** on Dashboard, GPU, Energy, Network and Disk they widen with the window.
- **Network:** Per-app traffic keeps its height, so the panels below it no longer jump.

## 0.1.0 - 2026-10-16

The first release.

- `sysmand`, a collector run as a user service: every 2 seconds it samples CPU, memory and pressure, sensors, GPUs,
  disks, network and processes. It keeps a crash log forced to disk line by line, so the last readings before a freeze
  or power cut survive, stores history in SQLite with rollups kept for years, and serves live data over a Unix socket.
- `sysman`, a native GPUI app with ten tabs: Dashboard, Processes, GPU, Energy, Network, Disk with a Disk Map,
  Hardware, Explorer, Insights and Groups. It follows the system font, offers light and dark appearances that can follow
  the desktop, and scales its text with a Regular, Bold or Bigger size.
- `sysman-helper`, an optional root service, off by default: SMART drive health, CPU package power, and per-process
  disk I/O and network traffic through eBPF.
- Hardware support across vendors: NVIDIA, AMD and Intel GPUs, and AMD and Intel CPUs.
- Packages: `sysman` and `sysman-bin` on the AUR, a `.deb` for Ubuntu 22.04 or newer and Debian 12 or newer, and
  `install.sh` for other distributions.
