# Hardware data sources for sysman (Linux, unprivileged)

Scope: NVIDIA/AMD/Intel(i915, xe) GPUs; x86 AMD/Intel CPUs; no root, no CAP_PERFMON, `perf_event_paranoid=2`.

## Evidence legend and sources

- **[DOC]** kernel documentation or the kernel source that backs it. The docs.kernel.org pages are kernel-doc stubs, so I read the `.rst` and the `.c` files they include. Base: `https://github.com/torvalds/linux/blob/master/` (fetched at master, 2026-10-15; not pinned to a release).
  - `D/gpu/drm-usage-stats.rst`, `D/gpu/amdgpu/{driver-misc,thermal}.rst`, `D/gpu/i915.rst`, `D/admin-guide/pm/{amd-pstate,intel_pstate}.rst`, `D/hwmon/{k10temp,coretemp}.rst`, `D/power/powercap/powercap.rst` (D = `Documentation/`).
  - ABI: `D/ABI/testing/sysfs-driver-intel-{xe,i915}-hwmon`, `sysfs-class-powercap`, `sysfs-devices-system-cpu`.
  - Source: `drivers/gpu/drm/amd/pm/amdgpu_pm.c`, `amd/amdgpu/amdgpu_fdinfo.c`, `xe/{xe_drm_client,xe_gt_freq,xe_gt_throttle,xe_gt_idle,xe_hwmon}.c`, `i915/i915_drm_client.c`, `i915/gt/intel_gt_sysfs_pm.c`, `i915/i915_hwmon.c`, `hwmon/k10temp.c`, `powercap/powercap_sys.c`.
- **[BESZEL]** beszel agent source, `henrygd/beszel` shallow clone (HEAD as of 2026-10-15), paths relative to `agent/`, with line numbers. Beszel uses gopsutil v4.26.9 (`go.mod:21`). I read gopsutil's `sensors/sensors_linux.go` at that tag.
- **[NVML]** `nvml-wrapper` 0.13.0 source from crates.io (`src/device.rs`, `src/struct_wrappers/device.rs`, `src/bitmasks/device.rs`). docs.rs URLs for 0.13.0 returned 404 for me, so I read the crate source, which is where the docs are generated from.
- **[LOCAL]** I checked this dev machine (Ryzen 9 7950X3D, Raphael iGPU `card0`, RTX 4090 on proprietary driver `card1`, kernel 7.2.9). Facts here are observed, not documented.
- **[UNCERTAIN]** from memory or inference. Verify before relying on it.

Nothing was run from the beszel clone or the crate. They were only read.

---

## 1. NVIDIA (NVML via `nvml-wrapper` 0.13)

The proprietary driver exposes no hwmon or `gpu_busy_percent` sysfs. [LOCAL] `card1/device` has only `power_state` and the PCI link files. Use NVML (`libnvidia-ml.so.1`, loaded at runtime). Nouveau is out of scope.

| Field | API (all on `Device`) | Units | No root? | Caveats |
|---|---|---|---|---|
| Utilisation % | `utilization_rates()` -> `Utilization{gpu, memory}` [NVML device.rs:4352; struct_wrappers/device.rs:328] | `gpu` = % of the sample period with a kernel running; `memory` = % of time memory was read or written | Yes [UNCERTAIN: NVML read calls are generally unprivileged] | `memory` is memory-controller busy, not capacity. Beszel uses only `.gpu` (gpu_nvml.go:188-195). |
| VRAM used/total | `memory_info()` [device.rs:2691] | bytes | Yes | Beszel prefers the `_v2` symbol if present (gpu_nvml.go:105-110). I did not check whether the 0.13 wrapper has a `memory_info_v2`. Beszel skips this call when util is 0 so it does not leave D3cold (gpu_nvml.go:203-230). |
| Temperature | `temperature(TemperatureSensor::Gpu)` [device.rs:4055; enum_wrappers/device.rs:400] | deg C (integer) | Yes | `Gpu` is the only variant in 0.13. `temperature_threshold(..)` [4085] gives slowdown/shutdown limits. |
| Power, instant | `power_usage()` [device.rs:3390] | milliwatts | Yes | Reports N/A on some GPU/driver combos (RTX 4060, driver 580.x). Beszel falls back to `nvmlDeviceGetSamples` and averages the samples since the last timestamp (gpu_nvml.go:232-241, 251-290). I did not confirm the 0.13 wrapper exposes samples; check `field_values_for` [3681] or the sys crate. |
| Energy counter | `total_energy_consumption()` [3418] | millijoules since driver load, u64 | Yes | Volta+. Differentiate for average power. At 1 W resolution u64 mJ will not wrap in practice. |
| Power limit | `power_management_limit()` [3249], `enforced_power_limit()` [1751], `power_management_limit_default()` [3208], `power_management_limit_constraints()` [3278] | milliwatts | Reads yes | `enforced` is what the driver applies after other limits. Setting needs root. |
| Fan | `num_fans()` [2106], `fan_speed(idx)` [1896], `fan_speed_rpm(idx)` [1934] | `fan_speed` = % of max (intended speed, not measured); `fan_speed_rpm` = RPM | Yes | The % is the commanded value. `fan_speed_rpm` is new in 0.13 and driver-version dependent [UNCERTAIN]. Fanless or laptop parts return `NotSupported`. |
| Core clock | `clock_info(Clock::Graphics or SM)` [604], `max_clock_info` [2545] | MHz | Yes | `Clock` has Graphics, SM, Memory, Video. |
| Memory clock | `clock_info(Clock::Memory)` | MHz | Yes | |
| Video enc/dec util | `encoder_utilization()` [1584], `decoder_utilization()` [1235] -> `UtilizationInfo` | % plus sampling period in microseconds | Yes | |
| Throttle reasons | `current_throttle_reasons()` [3806] (`_strict` variants too), `supported_throttle_reasons()` [3883] | `ThrottleReasons` u64 bitflags | Yes | Flags include GPU_IDLE, SW_POWER_CAP, HW_SLOWDOWN, SYNC_BOOST, APPLICATIONS_CLOCKS_SETTING [bitmasks/device.rs:13-46]. The non-`_strict` calls tolerate unknown bits. |
| PCIe link | `current_pcie_link_gen/width()` [1176, 1206], `max_pcie_link_gen/width()` [2578, 2611], `pcie_throughput(Send or Receive)` [3112] | gen, lanes; throughput in KB/s | Yes | Throughput is sampled over a short window (~20 ms per the doc comment). The link may be downtrained at idle. Sysfs `current_link_speed`/`current_link_width` under the PCI device also works [LOCAL]. |
| Per-process memory | `running_graphics_processes_v2()` [2242], `running_compute_processes_v2()` [768] -> `ProcessInfo{pid, used_gpu_memory(bytes), ..}` [struct_wrappers/device.rs:233] | bytes | [UNCERTAIN] NVML lists all PIDs for read-only queries on bare metal. In containers or PID namespaces the PIDs are wrong. | `UsedGpuMemory` can be "Unavailable". |
| Per-process util | `process_utilization_stats(last_seen_ts)` [device.rs:2323] -> samples `{pid, timestamp(us), sm_util, mem_util, enc_util, dec_util}` [struct_wrappers/device.rs:509] | % | [UNCERTAIN] I believe no root, Maxwell+. | Only returns processes with non-zero activity in the buffered window. Pass the last timestamp to avoid repeats. `accounting_stats_for(pid)` [5114] needs accounting mode, which is enabled by root [UNCERTAIN]. |
| Device identity | `pci_info()` [3047] (bus id string), `uuid()` [4313] | | Yes | |

Beszel pitfalls for NVML [BESZEL gpu_nvml_linux.go:27-57, gpu_nvml.go:181-195]:
- Before any NVML call it reads `/sys/bus/pci/devices/<bdf>/power/runtime_status` and `.../drm/card*/device/power_state`. If the status is not `active` or `resuming`, or the state is not `D0`, it skips the device and reports temperature and memory as 0. NVML calls can wake a suspended hybrid-laptop dGPU out of D3cold.
- If it cannot read these files, it assumes the GPU is active.
- The name is stripped of the `NVIDIA ` prefix and the ` Laptop GPU` suffix.

---

## 2. AMD (amdgpu, sysfs)

All paths are under `/sys/class/drm/cardN/device/` (call it `$D`). hwmon is at `$D/hwmon/hwmonM/`. Vendor check: `$D/vendor == 0x1002` [BESZEL gpu_amd_linux.go:37, 95]. Skip `card*-*` connector entries (gpu_amd_linux.go:57-60).

| Field | Path | Units | No root? | Caveats |
|---|---|---|---|---|
| Utilisation % | `$D/gpu_busy_percent` [DOC amdgpu_pm.c:1467 "DOC: gpu_busy_percent"] | 0-100, firmware-computed aggregate over IP cores | Yes [LOCAL 0444] | Instantaneous SMU value, so it is noisy. Beszel averages samples over its own polling (gpu.go:356-395). Returns an error while the GPU is runtime-suspended [DOC amdgpu_pm.c:182-200, `amdgpu_pm_get_access_if_active` returns -EPERM; I saw it used on `gpu_metrics`/`pcie_bw`, not confirmed for `gpu_busy_percent`]. |
| Memory-controller busy | `$D/mem_busy_percent` [DOC amdgpu_pm.c:1491] | % | Yes | dGPU only [UNCERTAIN]. Not present on my iGPU [LOCAL]. |
| VRAM used/total | `$D/mem_info_vram_used`, `mem_info_vram_total`, also `mem_info_vis_vram_*` (CPU-visible part) [DOC driver-misc.rst] | bytes | Yes | On an APU the VRAM is a small BIOS carve-out (512 MiB on my iGPU [LOCAL]). |
| GTT / shared | `$D/mem_info_gtt_used`, `mem_info_gtt_total` [DOC] | bytes | Yes | GTT is system RAM mapped for the GPU. [LOCAL] iGPU GTT total is about 62 GiB, so it is a limit, not a reservation. Beszel adds GTT to VRAM for used and total when `gtt_used > 0` (gpu_amd_linux.go:108-114, a comment cites beszel issue #1569). Consider keeping them separate. |
| Temperature | `hwmon/temp1_input` = edge, `temp2_input` = junction/hotspot, `temp3_input` = memory (temp2/3 are SOC15 dGPUs only); `tempN_label` gives edge/junction/mem; `_crit`, `_emergency` [DOC amdgpu_pm.c:3566-3582] | millidegree C | Yes | Beszel reads `temp1_input` only (gpu_amd_linux.go:119). [LOCAL] iGPU has only temp1 "edge". |
| Power | `hwmon/power1_average` (SoC average) and `power1_input` (instant) [DOC amdgpu_pm.c:3591-3593] | microwatts | Yes | Not every product has both. The driver hides whichever the firmware reports `-EOPNOTSUPP` for (amdgpu_pm.c:3823-3834). [LOCAL] iGPU exposes only `power1_input`. **On APUs this includes the CPU** (the DOC says so). No energy counter. Beszel sums power across all hwmon dirs of the card, preferring `average` and falling back to `input` (gpu_amd_linux.go:116-129). |
| Power limit | `hwmon/power1_cap` (selected), `power1_cap_min`, `power1_cap_max` [DOC :3595-3599] | microwatts | Read: yes | `power1_cap` is writable by root only. dGPU only [UNCERTAIN]. |
| Fan | `hwmon/fan1_input` (RPM), `pwm1` (0-255), `pwm1_enable`, `fan1_min/max` [DOC :3601-3622] | RPM, PWM | Yes | Absent on iGPUs and most laptops. |
| Core clock | `hwmon/freq1_input` (label `sclk`) [DOC :3626] | **hertz** | Yes | [LOCAL] 600000000. Alternative: `$D/pp_dpm_sclk`, lines like `1: 800Mhz *`, where `*` marks the current level [DOC amdgpu_pm.c:974-995]. The `S:` prefix is a deep-sleep level. |
| Memory clock | `hwmon/freq2_input` (label `mclk`, dGPU only [DOC :3628]); `$D/pp_dpm_mclk` | Hz; MHz in pp_dpm | Yes | |
| Video decode/encode util | `$D/vcn_busy_percent` [DOC amdgpu_pm.c:1515] | % | Yes [LOCAL present] | VCN is shared encode and decode. Not split. Per-process split is in fdinfo (`dec`, `enc`, `jpeg`). Older ASICs without VCN lack the file. |
| Throttle reasons | `$D/gpu_metrics` (binary struct, versioned, includes a throttle status field) [DOC amdgpu_pm.c:1766 "DOC: gpu_metrics"] | bitmask | Yes [LOCAL present, 0444] | Needs a parser for several struct versions (v1_x, v2_x, v3_x). The kernel doc does not define the throttle bits. There is no simple text file for throttling. |
| PCIe link | `$D/current_link_speed`, `current_link_width`, `max_link_*` (PCI core sysfs, not amdgpu) [LOCAL] | "16.0 GT/s PCIe", lanes | Yes | The link downtrains at idle. `$D/pcie_bw` gives message counts and max payload only, and returns -ENODATA on APUs [DOC amdgpu_pm.c:1539-1570]. |
| Runtime PM state | `$D/power_state` ("D0"/"D3cold") and `$D/power/runtime_status` | string | Yes | Check before reading sensors on a dGPU to avoid waking it [UNCERTAIN for amdgpu; beszel does it for NVML only]. |
| Product name | `$D/product_name` (only some server cards) [DOC driver-misc.rst]; else look up `device`/`revision` in `/usr/share/libdrm/amdgpu.ids` | string | Yes | Beszel does exactly this (gpu_amd_linux.go:229-261). |
| Per-process util | fdinfo `drm-engine-{gfx,compute,dma,dec,enc,enc_1,jpeg,vpe}: N ns` [DOC amdgpu_fdinfo.c:105-111] | cumulative ns busy | Own-uid processes only | **amdgpu does not emit `drm-cycles-*`, `drm-total-cycles-*` or `drm-maxfreq-*`.** Utilisation = Δns / Δwall ns (no frequency normalisation). `drm-engine-capacity-*` is not emitted by amdgpu [UNCERTAIN: not in the file I read]. |
| Per-process memory | fdinfo `drm-total-/drm-shared-/drm-resident-/drm-purgeable-{vram,gtt,cpu,gds,gws,oa,doorbell,mmioremap}` with KiB; legacy `drm-memory-{vram,gtt,cpu}: N KiB` (deprecated alias of `drm-resident-`); `amd-evicted-vram`, `amd-requested-{vram,gtt}` [DOC amdgpu_fdinfo.c:68-100, drm-usage-stats.rst] | KiB | Own-uid only | Region key names are lowercase `vram`, `gtt`. |

Beszel notes (AMD):
- It polls sysfs every 3 s (gpu_amd_linux.go:46). It gives up after `maxFailureRetries = 5` consecutive polls with no usable data (gpu.go:34).
- "No data" means `gpu_busy_percent` and `mem_info_vram_used` both failed and temp == 0 (gpu_amd_linux.go:132-135).
- `rocm-smi` is the legacy path and is marked deprecated (gpu.go:596). Its JSON keys are in gpu.go:64-74. Not needed.
- AMD sysfs is only chosen automatically if `rocm-smi` is absent or `AMD_SYSFS=true` (gpu.go:718-725).

---

## 3. Intel i915 (integrated and older discrete)

Paths under `/sys/class/drm/cardN/` (call it `$C`). i915 has **no `gpu_busy_percent`**. Vendor id 0x8086 (BESZEL gpu_intel_sysfs_linux.go:112).

| Field | Path / source | Units | No root? | Caveats |
|---|---|---|---|---|
| Utilisation % | See "Intel busy %" below | | | |
| Memory | No VRAM/GTT sysfs in i915 that I could find [UNCERTAIN]. Beszel probes `mem_info_{vram,lmem,local_mem}_{used,total}` (gpu_intel_sysfs_linux.go:152-162) but I found no kernel source defining them, so treat those probes as speculative. Per-process: fdinfo `drm-total-<region>`/`drm-resident-<region>` (`system`, plus local memory on dGPU) via `show_meminfo` [DOC i915_drm_client.c:73-102]. | bytes (KiB/MiB suffix) | own-uid | iGPU shares system RAM. For a system-wide figure, sum fdinfo `drm-resident-system` over visible processes, or just report `/proc/meminfo`. Region names are driver-defined; I did not read the i915 region names. |
| Temperature | iGPU: none of its own, use the CPU package sensor. dGPU: `hwmon/temp1_input` "GPU package" [DOC sysfs-driver-intel-i915-hwmon] | millidegree C | Yes | hwmon only on discrete (DG1/DG2). Beszel scans `temp*_input` and takes the first > 0 (gpu_intel_sysfs_linux.go:233-243). |
| Power | `hwmon/energy1_input` "energy of device or gt" [DOC sysfs-driver-intel-i915-hwmon:66] | microjoules, monotonic | Yes | **Differentiate**: W = ΔµJ / Δs / 1e6. No instantaneous `power1_input`. Reset or wrap: treat `new < old` as a restart and drop that sample (BESZEL gpu_intel_sysfs_linux.go:195-215). Hwmon is dGPU-only, so iGPU gets no package energy here (use RAPL, which is root-only). |
| Power limit | `hwmon/power1_max` (PL1, RW), `power1_rated_max` (default TDP), `power1_max_interval` (ms), `power1_crit` (µW), `curr1_crit` (mA) [DOC sysfs-driver-intel-i915-hwmon] | µW | Read: yes | |
| Fan | `hwmon/fan1_input` [DOC ... :79, dated Nov 2024] | RPM | Yes | Only recent kernels, only some dGPUs. |
| Voltage | `hwmon/in0_input` | mV | Yes | |
| Core clock | `$C/gt/gt0/rps_act_freq_mhz` (actual), `rps_cur_freq_mhz` (requested), `rps_max_freq_mhz`, `rps_RP0_freq_mhz`, `rps_RPn_freq_mhz`, `rps_boost_freq_mhz` [DOC intel_gt_sysfs_pm.c:315-406, 377-385]; legacy `$C/gt_act_freq_mhz` | MHz | Yes | One dir per GT (`gt0` render, `gt1` media on MTL). Frequency may read 0 when in RC6 [UNCERTAIN]. |
| Memory clock | none | | | |
| Idle residency | `$C/gt/gt0/rc6_residency_ms`, `media_rc6_residency_ms`; also `power/rc6_residency_ms` under the device [DOC intel_gt_sysfs_pm.c:199-228, 247-262] | cumulative ms (rc6 counter is u32 in µs internally, so it **wraps**, see `get_residency` at :170) | Yes | See Intel busy %. |
| Throttle | `$C/gt/gt0/throttle_reason_{status,pl1,pl2,pl4,thermal,prochot,ratl,vr_thermalert,vr_tdc}` [DOC intel_gt_sysfs_pm.c:537-556] | 0/1 each | Yes [UNCERTAIN: mode is 0444 per the RO macro] | Created only on platforms with the perf-limit registers (gated at :917). |
| Video enc/dec | fdinfo `drm-engine-video`, `drm-engine-video-enhance` plus `drm-engine-capacity-video` [DOC i915.rst:549-558] | ns | own-uid | No system-wide sysfs counter. |
| PCIe link | PCI sysfs `current_link_speed`/`current_link_width` (dGPU only meaningful) | | Yes | |
| Per-process | fdinfo `drm-engine-{render,copy,video,video-enhance}: N ns` + `drm-engine-capacity-<class>` (printed only when > 1) [DOC i915_drm_client.c:140-182; skipped on gen < 8] | cumulative ns | own-uid | Dedup by `drm-client-id` (same client via dup'ed fds shows once per fd) [DOC drm-usage-stats.rst]. |

### Intel busy % (i915 and xe): every unprivileged option

| Option | How | Accuracy and limits |
|---|---|---|
| A. fdinfo engine time, summed over own processes | i915: Δ`drm-engine-<class>` ns / (Δwall ns × capacity), per class; GPU "busy" = max over classes (same as beszel's reading of intel_gpu_top, gpu.go:415-428). xe: Δ`drm-cycles-<class>` / Δ`drm-total-cycles-<class>` / capacity. | Best unprivileged signal. Blind to other users' processes (fdinfo of other uids is unreadable without root/CAP_SYS_PTRACE [UNCERTAIN]) and to processes whose fd you cannot see (containers, sandboxes). Includes the compositor, which is where most desktop GPU work lands. Counters are per client and **can drop when a client exits** (its time is lost): track each `(pid, drm-client-id)` separately and clamp deltas to >= 0 (the kernel doc allows brief non-monotonic values and says userspace should hold the larger previous value). i915 `drm-engine` = busy time ignoring frequency (doc: it "only reflects time active without considering what frequency the engine is operating"). Reading fdinfo on xe wakes the device and takes forcewake (xe_drm_client.c `show_run_ticks`), so polling keeps an idle dGPU out of D3. |
| B. RC6 / GT idle residency | i915: `rc6_residency_ms`; xe: `gtidle/idle_residency_ms`. busy% ≈ 100 × (1 − Δresidency / Δwall_ms). | Whole-GT, includes other users' work (unlike A). It measures "powered up", not "executing": any activity keeps the GT out of RC6 and there is hysteresis before entering it, so light loads read as high busy. Use as an activity indicator, not a percentage. Handle i915 u32 µs wrap. Per-GT only: render and media are separate counters on MTL+. |
| C. Frequency as a proxy | `rps_act_freq_mhz` / `rps_max_freq_mhz` (i915); `freq0/act_freq` / `max_freq` (xe) | Weak. Frequency follows load but with lag, and at light load it is clamped to the efficient frequency. Use only to show a clock, not busy. |
| D. `intel_gpu_top` / perf PMU (`i915`, `xe` PMUs) | beszel runs `intel_gpu_top -s 3300 -J` and takes the max engine `busy` (gpu_intel.go:56-93, gpu.go:415-428) | Most accurate (hardware counters), but the PMU needs `perf_event_paranoid <= 0` or CAP_PERFMON [UNCERTAIN, from memory of intel_gpu_top docs]. **Not available under your constraints** unless the admin lowers paranoid or grants the capability to the binary. |

---

## 4. Intel xe (Arc discrete, Lunar Lake and newer iGPUs)

Detection: `DRIVER=xe` in `$C/device/uevent` [BESZEL gpu_nvtop.go:178]. Hwmon chip name is `xe` [BESZEL gpu_intel_sysfs_linux.go:122]. Paths: `$X = /sys/class/drm/cardN/device`.

| Field | Path / source | Units | No root? | Caveats |
|---|---|---|---|---|
| Utilisation % | Option A or B above | | | `drm-cycles-<class>` / `drm-total-cycles-<class>` classes: `rcs` (render), `ccs` (compute), `bcs` (copy), `vcs` (video), `vecs` (video enhance) [DOC xe_drm_client.c:59-73]. Both counters are printed together or not at all (omitted for SR-IOV VFs, `show_run_ticks`). `drm-engine-capacity-<class>` only when > 1. `total-cycles` is a GPU timestamp, so the ratio is independent of CPU sleep time. xe does not emit `drm-engine-<class>` ns [DOC; example output in xe_drm_client.c]. |
| Idle residency | `$X/tile0/gt0/gtidle/idle_residency_ms`, `idle_status` ("gt-c0"/"gt-c6"), `name` ("gt0-rc" render, "gt0-mc" media) [DOC xe_gt_idle.c:28-30, 62-65, 375-385] | ms, string | Yes | Source comment says `device/gt#/gtidle` but the code attaches it to the GT kobject, which lives under `tile#/gt#` (`xe_gt_freq.c:33` states the tile path for freq0). Glob `tile*/gt*/gtidle`. |
| Core clock | `$X/tile0/gt0/freq0/act_freq` (PCODE-resolved), `cur_freq` (GuC request), `min_freq`, `max_freq` (RW), `rp0_freq`, `rpe_freq`, `rpn_freq`, `rpa_freq` [DOC xe_gt_freq.c:21-51] | MHz [UNCERTAIN: `sysfs_emit("%d")` of the GuC PC value, MHz in practice] | Yes (read) | Glob `tile*/gt*/freq0`; media GT is a separate `gt1`. |
| Memory clock | none documented | | | |
| Throttle | `$X/tile0/gt0/freq0/throttle/{status, reasons, reason_pl1, reason_pl2, reason_pl4, reason_thermal, reason_prochot, reason_ratl, reason_vr_thermalert, reason_vr_tdc}`; Crescent Island platform adds soc/mem/vr thermal, iccmax, pwrbrk, psys_* [DOC xe_gt_throttle.c:18-62] | `status` 0/1; `reasons` space-separated names or `none` | Yes (0444) | The doc recommends `reasons` over `status` (reading `status` then individual files is racy). Reasons vary by platform. |
| Power (card) | `hwmon/energy1_input` label "card"; (package) `energy2_input` label "pkg" [DOC sysfs-driver-intel-xe-hwmon; xe_hwmon.c:1426-1450] | µJ | Yes | Differentiate. The kernel already accumulates the 32-bit hardware register into a wide value (xe_hwmon.c:485-543), so userspace wrap is not a practical concern but a counter reset on driver reload is. The file is only visible if the energy value is non-zero (xe_hwmon.c:1248-1257), so existence can change after resume. Beszel treats `energy1` as GPU power and the first other `energy*_input` as package power (gpu_intel_sysfs_linux.go:216-231); that ordering relies on glob order, so key by label instead. There is no `power1_input`. |
| Power limit | `power1_max` (card PL1, RW), `power1_rated_max`, `power1_crit`, `power1_max_interval`; `power2_max` (package PL1), `power2_rated_max`, `curr1_crit` [DOC sysfs-driver-intel-xe-hwmon] | µW, ms, mA | Read: yes | "Only supported for particular Intel Xe graphics platforms." |
| Temperature | `temp2_input` pkg, `temp3_input` vram, `temp4_input` mctrl (memory controller average), `temp5_input` pcie, `temp[6-21]_input` per-VRAM-channel; each with `_crit`, `_emergency` (and `temp2_max`). Labels `pkg`, `vram`, `mctrl`, `pcie`, `vram_ch_N` [DOC sysfs-driver-intel-xe-hwmon; xe_hwmon.c:1426-1443] | millidegree C | Yes | Select by `tempN_label`, not by index. Beszel takes the first positive `temp*_input` (gpu_intel_sysfs_linux.go:233-243), which is not guaranteed to be the package. Each hwmon read is a runtime-PM resume and can wake an idle card (BESZEL gpu.go:753-756 comment). |
| Fan | `fan1_input`, `fan2_input` (RPM) [DOC] | RPM | Yes | On DG2 two channels are always shown; two fans may share one tach line so channel 2 can duplicate or read 0 [DOC sysfs-driver-intel-xe-hwmon]. |
| Voltage | `in1_input` (package) | mV | Yes | |
| Memory used/total | No sysfs I could find [UNCERTAIN]. Per-process fdinfo: `drm-total-/shared-/active-/resident-/purgeable-{system,gtt,vram0,stolen}` [DOC xe_drm_client.c:42-58]. Device total VRAM via the DRM query ioctl on the render node (needs access to `/dev/dri/renderD*`, normally group `render`) [UNCERTAIN]. | bytes with KiB/MiB | own-uid | Beszel only sets memory if the `mem_info_*` probes happen to succeed, so on xe it usually reports none. |
| PCIe link | PCI sysfs `current_link_speed`/`width` | | Yes | |
| Encode/decode | fdinfo `vcs` (video), `vecs` (enhance) cycles | | own-uid | No sysfs. |
| Per-process | fdinfo as above | | own-uid | `drm-pdev` gives the PCI slot for matching to a card. |

xe pitfalls seen in beszel:
- `nvtop` leaves `device_name` empty on xe, so beszel synthesises "Intel GPU (<pci device id>)" from `device/device` (gpu_nvtop.go:54-60, 189-201).
- `intel_gpu_top` is skipped when xe is present (`caps.hasIntelGpuTop && !caps.hasXe`, gpu.go:728).

---

## 5. DRM fdinfo: key reference and driver support [DOC drm-usage-stats.rst]

- File: `/proc/<pid>/fdinfo/<fd>` for each fd whose `drm-driver:` line is present. The kernel doc says `drm-driver` is mandatory and `drm-pdev` (PCI slot, e.g. `0000:03:00.0`) and `drm-client-id` are optional.
- Engines: `drm-engine-<k>: N ns`, `drm-engine-capacity-<k>`, `drm-cycles-<k>`, `drm-total-cycles-<k>`, `drm-maxfreq-<k>`. The doc says a driver may implement `total-cycles` or `maxfreq`, not both.
- Memory: `drm-total-/shared-/resident-/purgeable-/active-<region>` (bytes by default, optional KiB/MiB suffix); `drm-memory-<region>` deprecated, printed only by amdgpu.
- Monotonicity: the doc says values may briefly decrease; userspace should keep the larger previous value until a monotonic update arrives.

| Key family | amdgpu | i915 | xe |
|---|---|---|---|
| `drm-engine-<k>` (ns) | yes (gfx, compute, dma, dec, enc, enc_1, jpeg, vpe) | yes (render, copy, video, video-enhance) | no |
| `drm-engine-capacity-<k>` | not seen in source | yes if > 1 | yes if > 1 |
| `drm-cycles-` / `drm-total-cycles-` | no | no | yes |
| `drm-total/resident/...-<region>` | yes (vram, gtt, cpu, ...) | yes (via `show_meminfo`) | yes (system, gtt, vram0, stolen) |
| `drm-memory-<region>` | yes (legacy) | no | no |

NVIDIA proprietary does not use DRM fdinfo; use NVML process APIs.

Caveats for any fdinfo scraper:
- Visibility: you can read `/proc/<pid>/fdinfo` only for processes you may ptrace-read (same uid in practice) [UNCERTAIN on exact policy]. Flatpak/bwrap-sandboxed apps still run as your uid, so they are visible.
- Duplicate fds for one client: key on `(drm-pdev, drm-client-id)`.
- Sum of process engine time does not equal device busy time, because engines run concurrently and clients overlap.
- Polling cost grows with the number of processes; scan `/proc/*/fd` for `/dev/dri/*` links only for processes whose `comm` or fd list shows DRM use, and cache fd-to-client mappings.
- Pid reuse and exits: keep per-client last values to compute deltas.

---

## 6. CPU: temperature, power, topology

### 6.1 Temperature [DOC k10temp.rst, coretemp.rst, k10temp.c; LOCAL]

| Source | Labels / files | Notes |
|---|---|---|
| `k10temp` (AMD Zen) | `temp1_input` = **Tctl**; `Tccd1..Tccd16` (`temp3`+; up to 8 per doc, 16 in k10temp.c:196-213); **Tdie** only on a hard-coded list of Zen 1/Zen+ parts (Ryzen 5 1600X, 7 1700X/1800X/2700X, Threadripper 19xx/29xx), where `temp2` = Tdie and Tctl is offset (k10temp.c:123-130, 537-545) | [LOCAL] 7950X3D: `temp1` Tctl, `temp3` Tccd1, `temp4` Tccd2, **no Tdie**. On Zen 2+ treat Tctl as the package temperature. `Tctl` has no real per-core data. Per-core: **not available on AMD**. Which CCD is `Tccd1` vs CCD0: assumed same order [UNCERTAIN]. `temp1_max`/`crit` are not shown on Zen (k10temp.c:306-308). |
| `zenpower` / `zenpower3` (out-of-tree, not in mainline) | Tdie, Tctl, Tccd#, plus SVI2 voltage and current/power channels | Not documented in kernel docs (the page 404s). Do not depend on it; accept it if present [UNCERTAIN]. |
| `coretemp` (Intel) | `tempN_label` = **"Package id Y"** (one per socket) and **"Core X"** (per physical core); `tempN_crit` = TjMax; `_crit_alarm` [DOC coretemp.rst:52-60] | Per-core temperatures exist on Intel (not per SMT thread). On hybrid parts every core, P and E, gets a "Core X" entry [UNCERTAIN]. Values are millidegree C. |
| `thermal_zone*` (`x86_pkg_temp`, `acpitz`) | `/sys/class/thermal/thermal_zone*/type` and `temp` | Fallback only. gopsutil reads these if hwmon has nothing (gopsutil sensors_linux.go:30-57). |

gopsutil key format used by beszel: `<hwmon name>` plus `_<label lowercased, spaces to underscores>`, e.g. `k10temp_tctl`, `k10temp_tccd1`, `coretemp_package_id_0`, `coretemp_core_3`, `nvme_composite` [gopsutil sensors_linux.go:80-119 v4.26.9]. A sensor without a label is keyed by the bare chip name. Beszel appends `_<index>` on key collisions (sensors.go:162-166). If you do your own hwmon scan, key on label so that two chips with the same name (two nvme, several `spd5118`) stay distinct.

Beszel sensor pitfalls [BESZEL sensors.go]:
- Whole collection runs under a timeout (default 2 s; 10 s on first run) in a goroutine, with panic recovery and one retry (sensors.go:185-223). A stuck hwmon read (typically a GPU or a broken driver) must not block the loop.
- Readings with `0 < t < 1` are rescaled by a heuristic (x100 or x1000 if it lands in 15-95 C) (sensors.go:155-157, 255-269). This works around drivers that report in odd units. Readings <= 0 or >= 200 are dropped (159-161).
- `SKIP_GPU` builds a temporary shadow `/sys` tree without GPU hwmon chips (`xe`, `i915`, `amdgpu`, `radeon`, `nvidia`, `nouveau`) because the reads themselves wake the GPU (sensors.go:82-94, 302-368; gpu.go:753-777). Same for fans (fans.go:37-40).
- Fans: scans `fan*_input` in every hwmon dir, falling back to the `device/` subdirectory for applesmc-style drivers; zero RPM is kept (fans.go:71-120).

### 6.2 Package power / RAPL [DOC powercap_sys.c, sysfs-class-powercap; LOCAL]

| Item | Finding |
|---|---|
| Path | `/sys/class/powercap/intel-rapl:0/` (package, `name` = `package-0`), `intel-rapl:0:0` (core/uncore/dram sub-zones), `intel-rapl:1` for socket 1 or psys [DOC powercap.rst:35-100] |
| Files | `energy_uj` (µJ counter), `max_energy_range_uj` (wrap point), `name`, `constraint_N_power_limit_uw`, `_time_window_us`, `_max_power_uw` [DOC sysfs-class-powercap] |
| Permissions | `energy_uj` is created `S_IRUSR` = **0400 root only** (powercap_sys.c:367-373). [LOCAL] `-r-------- root`, while `name` and `max_energy_range_uj` are 0444. This is the mitigation for the Platypus power side channel [UNCERTAIN: reason stated from memory; the mode itself is confirmed in source]. |
| AMD | The same `intel-rapl` powercap tree is used on Zen [LOCAL], same root-only mode. |
| Distro practice | By default no distro makes `energy_uj` readable by users [UNCERTAIN, not documented]. To read it you need a udev rule or `systemd-tmpfiles` `z` rule to chmod/chgrp it, or running a helper with privileges. A user service cannot do this itself. |
| Unprivileged alternatives | (a) amdgpu `power1_input` on an APU (includes the CPU) as a coarse SoC power [DOC amdgpu_pm.c:3591]; (b) battery `power_now` for total system draw on laptops [UNCERTAIN]; (c) perf RAPL PMU (`/sys/devices/power/events/energy-pkg` exists [LOCAL]) needs system-wide perf events, which `perf_event_paranoid=2` blocks. Verdict: **no unprivileged CPU package power** on a default setup; design the field as optional. |
| Counter handling | Compute W = Δenergy_uj / Δt; on `new < old` add `max_energy_range_uj` once (wrap) or drop the sample; the range is small on some platforms (tens of seconds at high power) so poll faster than the wrap time. |

### 6.3 Topology and ranking

| Need | Source | Findings |
|---|---|---|
| Intel hybrid P/E | `/sys/devices/cpu_core/cpus` (P-cores) and `/sys/devices/cpu_atom/cpus` (E-cores), cpulist format | perf PMU directories that exist only on hybrid CPUs [UNCERTAIN: not in the kernel docs I fetched; confirmed absent on this AMD box [LOCAL]]. Meteor Lake and later may add a third low-power E-core PMU (`cpu_lowpower`) [UNCERTAIN]. Fallback: per-CPU `cpuinfo_max_freq` / `base_frequency` differ by core type under intel_pstate. `intel_pstate` hybrid support needs HWP and treats P/E uniformly in HWP units [DOC intel_pstate.rst:339-350]. There is no `cpu_capacity` file on x86 (not in the ABI file I grepped). |
| AMD CCD / L3 grouping | `/sys/devices/system/cpu/cpuN/cache/index3/shared_cpu_list` (+ `size`, `id`) [DOC sysfs-devices-system-cpu:449 "list of logical cpus sharing the cache"] | [LOCAL] 7950X3D: `cpu0` -> `0-7,16-23`, `cpu8` -> `8-15,24-31`; `index3/size` is 98304K for CCD0 (3D V-Cache) and 32768K for CCD1, so the size also identifies the V-Cache CCD. **`topology/die_id` was 0 for every CPU and `cluster_id` 65535** [LOCAL], so on Zen 4 desktop `die_id` does not separate CCDs. Use L3 `shared_cpu_list` (or `index3/id`). Intel client CPUs have one L3 domain per package, so grouping is trivial there. |
| AMD preferred-core ranking | `/sys/devices/system/cpu/cpufreq/policyN/amd_pstate_prefcore_ranking` (larger = preferred, changes at runtime), `amd_pstate_hw_prefcore`, `amd_pstate_highest_perf`, `amd_pstate_max_freq`, `amd_pstate_lowest_nonlinear_freq`; global `/sys/devices/system/cpu/amd_pstate/prefcore` ("enabled"/"disabled") and `.../amd_pstate/status` [DOC amd-pstate.rst:236-280, 522] | [LOCAL] policy0 = 201, policy8 = 226; files 0444. The ranking and `hw_prefcore` files are present only on platforms with preferred core. The same numbers appear in the generic ACPI CPPC file `cpuN/acpi_cppc/highest_perf` (201/226 [LOCAL], 0444), which also works without `amd-pstate` (e.g. `acpi-cpufreq`) [UNCERTAIN beyond this machine]. `cpuinfo_max_freq` was identical across CCDs here, so it does not reveal the best cores. |
| Intel pstate | `/sys/devices/system/cpu/intel_pstate/{status,no_turbo,num_pstates,turbo_pct,hwp_dynamic_boost}`, per-CPU `cpufreq/base_frequency`, `energy_performance_preference` [DOC intel_pstate.rst:473-690] | Absent when `intel_pstate` is not the driver. |

---

## 7. How beszel identifies and matches GPUs

- There is **no cross-source PCI matching**. Each collector keys `GpuDataMap` by its own id: NVML/nvidia-smi use the index (`"0"`, `"1"`; gpu_nvml.go:166, gpu.go:253), AMD sysfs and Intel sysfs use the DRM node name (`card0`; gpu_amd_linux.go:99, gpu_intel_sysfs_linux.go:140), `intel_gpu_top` is hard-coded to `"i0"` ("only one gpu for now", gpu_intel.go:33-34), nvtop uses `"n<index>"` (gpu_nvtop.go:61), rocm-smi uses GUID (gpu.go:295).
- It avoids double-reporting with **one source per vendor group** (nvidia/intel/amd) chosen by priority; nvtop is used only if nothing else applies or it is listed first (gpu.go:666-704, 706-751). Two Intel GPUs under `intel_gpu_top` are not distinguishable except via the `INTEL_GPU_DEVICE` env var (`-d`) (gpu_intel.go:59-61).
- PCI address is used in exactly one place: NVML BusId lowercased -> `/sys/bus/pci/devices/<bdf>/...` for the runtime-PM check (gpu_nvml.go:136-140, gpu_nvml_linux.go:29-43).
- Names: duplicates get the id appended (`"<name> <id>"`, gpu.go:324-327). AMD name = `product_name` -> `amdgpu.ids` lookup by device/revision -> `AMD GPU (<devid>)`. Intel sysfs name = `product_name` -> `name` -> `Intel GPU cardN` (gpu_intel_sysfs_linux.go:268-280).
- **Recommendation for sysman**: key every GPU by PCI slot (`readlink $C/device` basename, or `drm-pdev` from fdinfo, or NVML `pci_info()` bus id) so that sysfs, hwmon, fdinfo and NVML data join on one id. Normalise case (NVML returns upper case with an 8-digit domain, e.g. `00000000:01:00.0`, versus sysfs `0000:01:00.0`; beszel lowercases but I did not verify the domain width mismatch handling).
- Averaging model: collectors add samples into accumulators (`Usage`, `Power`, `Count`); readers compute `delta/deltaCount` since their last read, with a separate snapshot per cache key (gpu.go:310-451). When no new samples arrived, discrete GPUs with zero temp and memory are reported as zeros (suspended), while engine-based Intel GPUs carry the last average forward (gpu.go:362-375).
- Retry and stuck tools: external tools are restarted after errors with a 5 s wait; after `maxFailureRetries = 5` it gives up (gpu.go:33-34, 144-157, 491-506). `intel_gpu_top -J`: first sample is discarded as unreliable (gpu_intel.go:106-107, 133-136); the JSON stream is parsed object by object because versions differ (`[` wrapper since v1.28, missing commas on some i915/Arrow Lake builds) (gpu_intel.go:95-100); power over 5000 W is rejected because the tool computes it from unsigned counter deltas, so a counter that goes backwards yields an enormous value (gpu_intel.go:262-268); the child is killed and reaped on every exit path (gpu_intel.go:73-84).
- Units: MiB-vs-MB fudge `1.024` for nvidia-smi (gpu.go:37), mW for tegrastats, µW for amdgpu hwmon, µJ for Intel energy; `utils.BytesToMegabytes` for sysfs bytes.
- Not in beszel: no RAPL, no `intel_pstate`/`amd_pstate`, no P/E detection, no CCD grouping, no fdinfo scraping, no per-process GPU stats, no clocks, throttle reasons, PCIe or video-engine data. Everything in sections 6.2-6.3 is new work for sysman.

---

## Gaps / unverifiable without hardware

- **Intel i915 and xe**: no Intel GPU here, so every i915 or xe path, unit, and file permission is from kernel source only. Check on hardware: xe `freq0/act_freq` unit (MHz assumed), whether `gt/gt0/throttle_reason_*` is world-readable, whether hwmon appears on iGPUs, the i915 fdinfo region names, whether xe exposes any VRAM-size sysfs, and what `temp1` is on xe (the ABI lists `temp2` onward).
- **Beszel's `mem_info_lmem_*` / `mem_info_local_mem_*` probes** have no kernel source I could find; they may be dead code.
- **NVML**: could not confirm (a) per-process util and PID visibility without root, (b) `fan_speed_rpm` driver requirements, (c) a `memory_info_v2` or power-samples API in nvml-wrapper 0.13, (d) NVML behaviour on a runtime-suspended dGPU (beszel's workaround suggests it wakes the GPU). My RTX 4090 is on this box but I did not run NVML.
- **amdgpu dGPU** (fan, `mem_busy_percent`, `power1_cap`, `temp2/3`, `freq2`, `pcie_bw`): only an iGPU is available here. `gpu_metrics` throttle bits are not decoded in the kernel docs.
- **CPU**: Intel hybrid dirs (`cpu_core`/`cpu_atom`/lowpower) and `coretemp` labels on hybrid parts are unverified. zenpower semantics unverified. `Tccd#` to physical CCD ordering unverified. Which distros ship a RAPL udev rule is unknown.
- **fdinfo permissions** for other-uid processes were not tested; no DRM client was running as another user on this box. I also did not capture a live fdinfo sample on the amdgpu iGPU.
- **Kernel version drift**: sources are from `master`, and ABI files (for example `fan1_input` for i915, throttle files) are newer than many distro kernels. Feature-detect by file existence, never by kernel version.
