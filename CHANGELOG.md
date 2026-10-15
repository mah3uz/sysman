# Changelog

## Unreleased

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
