<script setup>
import CodeBlock from './CodeBlock.vue'
import Icon from './Icon.vue'

const tiers = [
  { label: 'Every reading', detail: 'full resolution', span: '48 hours', width: '22%', color: 'bg-ok' },
  { label: '1-minute min / avg / max', detail: 'rollups', span: '90 days', width: '58%', color: 'bg-cool' },
  { label: '15-minute min / avg / max', detail: 'rollups', span: '2 years', width: '100%', color: 'bg-accent' },
]

const sources = ['Kernel', 'Hardware', 'Services', 'Apps', 'Scheduled jobs', 'Sign-ins', 'Boot and power', 'Crashes', 'Packages']

const findings = [
  { text: 'Crashes, folded per program, and OOM kills', dot: 'bg-hot' },
  { text: 'Failed units and restart loops', dot: 'bg-warn' },
  { text: 'Boots that ended without a shutdown, with the last readings', dot: 'bg-violet' },
  { text: 'GPU faults, disk and NVMe errors, throttling', dot: 'bg-cool' },
  { text: 'Failed sign-ins', dot: 'bg-accent' },
]

const logRows = [
  { time: '03:42:05', unit: 'kernel', text: 'firefox[48213]: segfault at 0 ip 00007f2c8e3a' },
  { time: '03:42:08', unit: 'systemd-coredump', text: 'Process 48213 (firefox) dumped core.', mark: 'bg-hot', selected: true },
  { time: '03:44:54', unit: 'NetworkManager', text: 'dhcp4 (enp5s0): state changed new lease' },
  { time: '03:53:27', unit: 'gnome-shell', text: 'Window manager warning: binding replaced', mark: 'bg-warn' },
]

const whyChain = [
  { text: 'cups.socket', depth: 0 },
  { text: 'cups.service · restarted 0 times', depth: 1 },
  { text: 'cupsd 1069', depth: 2, last: true },
]

const whyMore = [
  { text: 'Stop, Restart or Disable the unit, each confirmed, or open its logs', dot: 'bg-accent' },
  { text: 'The ports it listens on; search port:631 to find who holds a port', dot: 'bg-ok' },
  { text: 'Explorer and Logs say why a process that has exited ran', dot: 'bg-violet' },
]

const path = (values) => values.map((v, i) => `${i ? 'L' : 'M'}${(i * 100) / (values.length - 1)},${40 - v * 36}`).join(' ')
const recorded = [
  { label: 'CPU', value: '2%', dot: 'bg-accent', stroke: 'var(--blue)', d: path([0.18, 0.22, 0.15, 0.3, 0.2, 0.26, 0.62, 0.34, 0.22, 0.28, 0.2, 0.24]) },
  { label: 'CPU temperature', value: '40°C', dot: 'bg-warn', stroke: 'var(--orange)', d: path([0.4, 0.42, 0.38, 0.45, 0.41, 0.44, 0.58, 0.52, 0.46, 0.43, 0.42, 0.44]) },
]

const jsonl = [
  { time: '21:14:02', temp: 74, tone: 'text-ink' },
  { time: '21:14:04', temp: 81, tone: 'text-warn' },
  { time: '21:14:06', temp: 88, tone: 'text-warn' },
  { time: '21:14:08', temp: 94, tone: 'text-hot' },
]

const lanes = [
  { name: 'cpu', width: '34%' },
  { name: 'memory', width: '18%' },
  { name: 'gpu', width: '76%', hung: true },
  { name: 'disks', width: '26%' },
  { name: 'sensors', width: '44%' },
]

const crashLog =`f=~/.local/share/sysman/log/$(date +%F).jsonl
tail -1 "$f" | jq .  # the latest sample
jq -c 'select(.type=="sample") | [.time, .cpu.temp_c, .gpu.devices[]?.temp_c]' "$f"
jq -c 'select(.errors) | [.time, .errors]' "$f"  # collector failures`
</script>

<template>
  <section id="features" class="relative py-24 sm:py-32">
    <div class="wrap">
      <div class="max-w-2xl">
        <p v-reveal class="eyebrow">Why sysman</p>
        <h2 v-reveal="60" class="mt-3 text-3xl font-semibold tracking-tight text-balance sm:text-5xl">
          A monitor that keeps watching when you are not.
        </h2>
        <p v-reveal="120" class="mt-5 text-lg leading-relaxed text-muted">
          The collector runs as a small user service whether the app is open or not. Close the window,
          reboot, lose power: what happened is still there to look at.
        </p>
      </div>

      <div class="mt-14 grid gap-4 md:grid-cols-6">
        <!-- live -->
        <article v-reveal class="card relative overflow-hidden p-6 sm:p-7 md:col-span-3 lg:col-span-2">
          <div class="flex h-10 w-10 items-center justify-center rounded-xl bg-ok/12 text-ok"><Icon name="activity" /></div>
          <h3 class="mt-5 text-lg font-semibold">Live, every two seconds</h3>
          <p class="mt-2 leading-relaxed text-muted">
            CPU, memory, pressure, sensors, GPUs, disks and network every 2 seconds; every process every 10;
            filesystems, drives and batteries less often. The app subscribes over a local socket and
            redraws only the tab you are looking at.
          </p>
          <div class="mt-6 flex items-end gap-1" aria-hidden="true">
            <span
              v-for="i in 24"
              :key="i"
              class="bar flex-1 rounded-sm bg-ok/70"
              :style="{ '--i': i, height: `${12 + ((i * 37) % 23)}px` }"
            />
          </div>
        </article>

        <!-- history -->
        <article v-reveal="80" class="card relative overflow-hidden p-6 sm:p-7 md:col-span-3 lg:col-span-4">
          <div class="flex h-10 w-10 items-center justify-center rounded-xl bg-accent/12 text-accent"><Icon name="database" /></div>
          <h3 class="mt-5 text-lg font-semibold">Recorded history, years of it</h3>
          <p class="mt-2 max-w-xl leading-relaxed text-muted">
            Everything lands in a local SQLite database. Pick any range from five minutes to seven days on
            any screen, or open Explorer to chart any recorded metric or process side by side.
            Every process is recorded every 10 seconds, with a week of per-minute history.
          </p>
          <ul class="mt-6 space-y-3">
            <li v-for="t in tiers" :key="t.label" class="grid gap-1.5 lg:grid-cols-[13rem_1fr] lg:items-center lg:gap-4">
              <span class="text-sm"><span class="font-medium">{{ t.label }}</span> <span class="text-subtle">{{ t.detail }}</span></span>
              <span class="relative h-7 overflow-hidden rounded-md bg-canvas-2">
                <span class="tier absolute inset-y-0 left-0 rounded-md opacity-80" :class="t.color" :style="{ width: t.width }" />
                <span class="absolute inset-y-0 right-2.5 flex items-center font-mono text-xs font-medium text-ink">{{ t.span }}</span>
              </span>
            </li>
          </ul>
          <p class="mt-4 text-xs text-subtle">Default retention; each tier is configurable.</p>
        </article>

        <!-- logs -->
        <article v-reveal class="card relative overflow-hidden p-6 sm:p-7 md:col-span-6">
          <div class="grid gap-x-10 gap-y-6 lg:grid-cols-[minmax(0,1.1fr)_minmax(0,1fr)]">
            <div>
              <div class="flex h-10 w-10 items-center justify-center rounded-xl bg-cool/12 text-cool"><Icon name="list" /></div>
              <h3 class="mt-5 text-lg font-semibold">Logs beside the metrics</h3>
              <p class="mt-2 leading-relaxed text-muted">
                The Logs screen reads the system’s logs where they are and puts them on one timeline, and
                fixed rules pick out what is worth a look. Open any entry to see what the machine was doing at that
                moment: CPU, memory and temperature, and the crashed process’s own memory, with a jump to
                Explorer at that minute.
              </p>
            </div>
            <div>
              <div class="art" style="--tint: var(--teal)" aria-hidden="true">
                <div class="space-y-px font-mono">
                  <div
                    v-for="r in logRows"
                    :key="r.time"
                    class="grid grid-cols-[2px_auto_minmax(0,1fr)] items-center gap-2.5 rounded-md px-2 py-1 sm:grid-cols-[2px_auto_7.5rem_minmax(0,1fr)]"
                    :class="r.selected && 'bg-accent/15'"
                  >
                    <span class="h-3 rounded-full" :class="r.mark" />
                    <span class="text-subtle tabular-nums">{{ r.time }}</span>
                    <span class="hidden truncate sm:block">{{ r.unit }}</span>
                    <span class="truncate" :class="r.selected && 'text-ink'">{{ r.text }}</span>
                  </div>
                </div>
                <p class="art-label mt-3 border-t border-line px-2 pt-3">What sysman recorded</p>
                <div class="mt-2 grid grid-cols-2 gap-4 px-2">
                  <div v-for="c in recorded" :key="c.label">
                    <p class="flex items-center justify-between gap-2">
                      <span class="flex min-w-0 items-center gap-1.5"><span class="h-1.5 w-1.5 shrink-0 rounded-full" :class="c.dot" /><span class="truncate">{{ c.label }}</span></span>
                      <span class="font-mono text-ink">{{ c.value }}</span>
                    </p>
                    <div class="relative mt-1.5 h-9">
                      <svg viewBox="0 0 100 40" preserveAspectRatio="none" class="absolute inset-0 h-full w-full">
                        <path :d="c.d" fill="none" :stroke="c.stroke" stroke-width="1.5" stroke-linejoin="round" vector-effect="non-scaling-stroke" />
                      </svg>
                      <span class="absolute inset-y-0 left-[54.5%] w-px bg-ink/35" />
                    </div>
                  </div>
                </div>
              </div>
            </div>
          </div>
          <p class="mt-7 text-xs font-medium tracking-wide text-subtle uppercase">Worth a look</p>
          <ul class="mt-3 grid gap-x-8 gap-y-2.5 text-sm text-muted sm:flex sm:flex-wrap">
            <li v-for="f in findings" :key="f.text" class="flex gap-2.5"><span class="mt-2 h-1.5 w-1.5 shrink-0 rounded-full" :class="f.dot" />{{ f.text }}</li>
          </ul>
          <ul class="mt-6 flex flex-wrap gap-1.5" aria-label="Log sources">
            <li v-for="s in sources" :key="s" class="rounded-md border border-line bg-canvas-2/70 px-2.5 py-1 text-xs text-muted">{{ s }}</li>
          </ul>
        </article>

        <!-- why -->
        <article v-reveal class="card relative overflow-hidden p-6 sm:p-7 md:col-span-6">
          <div class="grid gap-x-10 gap-y-6 lg:grid-cols-[minmax(0,1.1fr)_minmax(0,1fr)]">
            <div>
              <div class="flex h-10 w-10 items-center justify-center rounded-xl bg-accent/12 text-accent"><Icon name="layers" /></div>
              <h3 class="mt-5 text-lg font-semibold">Why is this running?</h3>
              <p class="mt-2 leading-relaxed text-muted">
                Select a process and sysman says in one sentence why it exists: a service enabled at boot, a
                socket, timer or D-Bus activation, a login autostart entry, an app you opened, a command in a
                terminal, a container, cron or the kernel. It shows the chain from the cause to the process, and
                whether the process comes back if you end it. When sysman can’t tell, it says why rather than
                guessing.
              </p>
              <ul class="mt-6 space-y-2.5 text-sm text-muted">
                <li v-for="w in whyMore" :key="w.text" class="flex gap-2.5"><span class="mt-2 h-1.5 w-1.5 shrink-0 rounded-full" :class="w.dot" />{{ w.text }}</li>
              </ul>
            </div>
            <div>
              <div class="art" aria-hidden="true">
                <p class="art-label">Why it’s running</p>
                <p class="mt-2 text-[0.8rem] text-ink">Started when something connected to cups.socket.</p>
                <ol class="mt-2.5 space-y-1 font-mono">
                  <li
                    v-for="c in whyChain"
                    :key="c.text"
                    class="truncate"
                    :class="c.last && 'text-ink'"
                    :style="{ paddingLeft: `${c.depth * 0.9}rem` }"
                  ><span v-if="c.depth" class="text-subtle">└ </span>{{ c.text }}</li>
                </ol>
                <p class="art-label mt-3 border-t border-line pt-3">If you end it</p>
                <p class="mt-1.5">
                  After End it stays stopped until something connects to cups.socket. After Force end, systemd
                  starts it again right away.
                </p>
                <p class="mt-3 flex flex-wrap gap-1.5">
                  <span v-for="a in ['Stop', 'Restart', 'Disable', 'Logs for this unit']" :key="a" class="rounded-md border border-line bg-surface/60 px-2 py-0.5 text-ink">{{ a }}</span>
                </p>
                <p class="art-label mt-3 border-t border-line pt-3">Listening on</p>
                <p class="mt-1.5 flex justify-between gap-3 font-mono"><span><span class="text-subtle">TCP</span> 127.0.0.1:631</span><span class="font-sans">this machine only</span></p>
              </div>
            </div>
          </div>
        </article>

        <!-- crash log -->
        <article v-reveal class="card relative overflow-hidden p-6 sm:p-7 md:col-span-6 lg:col-span-4">
          <div class="grid gap-7">
            <div class="grid gap-x-10 gap-y-5 lg:grid-cols-[minmax(0,1.1fr)_minmax(0,1fr)]">
              <div>
                <div class="flex h-10 w-10 items-center justify-center rounded-xl bg-warn/12 text-warn"><Icon name="shield" /></div>
                <h3 class="mt-5 text-lg font-semibold">A crash log that survives the crash</h3>
                <p class="mt-2 leading-relaxed text-muted">
                  Every tick is written as one line of JSON and forced to disk before the next tick starts,
                  so a hard freeze or a power cut cannot take the last readings with it. Open the day's file
                  and read what led up to the gap.
                </p>
              </div>
              <div>
                <div class="art font-mono" style="--tint: var(--orange)" aria-hidden="true">
                  <p class="art-label font-sans">2026-10-20.jsonl</p>
                  <ol class="mt-2 space-y-1">
                    <li v-for="l in jsonl" :key="l.time" class="flex items-center gap-2.5 whitespace-nowrap">
                      <span class="min-w-0 flex-1 truncate">{"time":"<span class="text-ink">{{ l.time }}</span>",…,"temp_c":<span :class="l.tone">{{ l.temp }}</span>,…}</span>
                      <Icon name="check" :size="12" class="shrink-0 text-ok" />
                    </li>
                  </ol>
                  <div class="gap mt-2 flex h-10 items-center justify-center rounded-md border border-dashed border-line-strong font-sans text-subtle">nothing after 21:14:08</div>
                </div>
              <ul class="mt-6 space-y-2.5 text-sm text-muted">
                <li class="flex gap-2.5"><span class="mt-2 h-1.5 w-1.5 shrink-0 rounded-full bg-hot" />Temperatures climbing, fans falling: cooling.</li>
                <li class="flex gap-2.5"><span class="mt-2 h-1.5 w-1.5 shrink-0 rounded-full bg-warn" />Flat and normal to the last line: power supply or cabling.</li>
                <li class="flex gap-2.5"><span class="mt-2 h-1.5 w-1.5 shrink-0 rounded-full bg-violet" />A burst of GPU errors first: the GPU or its driver hung.</li>
              </ul>
              </div>
            </div>
            <CodeBlock class="min-w-0" :code="crashLog" label="Reading the log after a crash" />
          </div>
        </article>

        <!-- overhead -->
        <article v-reveal="80" class="card relative overflow-hidden p-6 sm:p-7 md:col-span-6 lg:col-span-2">
          <div class="flex h-10 w-10 items-center justify-center rounded-xl bg-violet/12 text-violet"><Icon name="gauge" /></div>
          <h3 class="mt-5 text-lg font-semibold">Light enough to forget</h3>
          <p class="mt-2 leading-relaxed text-muted">
            About 2% of one core with the defaults. Each collector runs on its own thread with a deadline,
            so a hung GPU driver or a slow sensor never holds up the rest; it is recorded as an error and
            the tick goes on.
          </p>
          <div class="art mt-6" style="--tint: var(--purple)" aria-hidden="true">
            <p class="art-label">One tick, one thread each</p>
            <div class="mt-2.5 grid grid-cols-[auto_1fr] gap-x-3">
              <div class="space-y-1.5 font-mono">
                <p v-for="l in lanes" :key="l.name" class="h-3 leading-3">{{ l.name }}</p>
              </div>
              <div class="relative space-y-1.5">
                <div v-for="l in lanes" :key="l.name" class="relative h-3">
                  <span class="absolute inset-y-0.5 left-0 rounded-full" :class="l.hung ? 'hung' : 'bg-violet/55'" :style="{ width: l.width }" />
                  <Icon v-if="l.hung" name="x" :size="12" class="absolute top-0 left-[76%] ml-1.5 text-hot" />
                </div>
                <span class="absolute -top-1.5 -bottom-1.5 left-[76%] border-l border-dashed border-hot/70" />
              </div>
            </div>
            <p class="mt-3 flex flex-wrap justify-between gap-x-3"><span><span class="text-hot">gpu</span> timed out: an error</span><span class="text-ok">the tick goes on</span></p>
          </div>
          <div class="mt-6 flex items-baseline gap-2">
            <span class="text-5xl font-semibold tracking-tight tabular-nums">~2%</span>
            <span class="text-sm text-subtle">of one core, measured</span>
          </div>
        </article>
      </div>
    </div>
  </section>
</template>

<style scoped>
.gap {
  background: repeating-linear-gradient(135deg, var(--line) 0 6px, transparent 6px 12px);
}
.hung {
  background: repeating-linear-gradient(135deg, color-mix(in oklab, var(--red) 60%, transparent) 0 4px, transparent 4px 8px);
  box-shadow: inset 0 0 0 1px color-mix(in oklab, var(--red) 55%, transparent);
}
.bar {
  transform-origin: bottom;
  animation: pulse 2s ease-in-out infinite;
  animation-delay: calc(var(--i) * -0.17s);
}
@keyframes pulse {
  0%,
  100% {
    transform: scaleY(0.55);
    opacity: 0.55;
  }
  50% {
    transform: scaleY(1);
    opacity: 1;
  }
}
</style>
