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

const crashLog = `f=~/.local/share/sysman/log/$(date +%F).jsonl
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
          <div class="grid gap-x-10 gap-y-6 lg:grid-cols-[1.1fr_1fr]">
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
            <div class="lg:self-end">
              <p class="text-xs font-medium tracking-wide text-subtle uppercase">Worth a look</p>
              <ul class="mt-3 space-y-2.5 text-sm text-muted">
                <li v-for="f in findings" :key="f.text" class="flex gap-2.5"><span class="mt-2 h-1.5 w-1.5 shrink-0 rounded-full" :class="f.dot" />{{ f.text }}</li>
              </ul>
            </div>
          </div>
          <ul class="mt-6 flex flex-wrap gap-1.5" aria-label="Log sources">
            <li v-for="s in sources" :key="s" class="rounded-md border border-line bg-canvas-2/70 px-2.5 py-1 text-xs text-muted">{{ s }}</li>
          </ul>
        </article>

        <!-- crash log -->
        <article v-reveal class="card relative overflow-hidden p-6 sm:p-7 md:col-span-6 lg:col-span-4">
          <div class="grid gap-7">
            <div class="grid gap-x-10 gap-y-5 lg:grid-cols-[1.1fr_1fr]">
              <div>
                <div class="flex h-10 w-10 items-center justify-center rounded-xl bg-warn/12 text-warn"><Icon name="shield" /></div>
                <h3 class="mt-5 text-lg font-semibold">A crash log that survives the crash</h3>
                <p class="mt-2 leading-relaxed text-muted">
                  Every tick is written as one line of JSON and forced to disk before the next tick starts,
                  so a hard freeze or a power cut cannot take the last readings with it. Open the day's file
                  and read what led up to the gap.
                </p>
              </div>
              <ul class="space-y-2.5 text-sm text-muted lg:self-end">
                <li class="flex gap-2.5"><span class="mt-2 h-1.5 w-1.5 shrink-0 rounded-full bg-hot" />Temperatures climbing, fans falling: cooling.</li>
                <li class="flex gap-2.5"><span class="mt-2 h-1.5 w-1.5 shrink-0 rounded-full bg-warn" />Flat and normal to the last line: power supply or cabling.</li>
                <li class="flex gap-2.5"><span class="mt-2 h-1.5 w-1.5 shrink-0 rounded-full bg-violet" />A burst of GPU errors first: the GPU or its driver hung.</li>
              </ul>
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
