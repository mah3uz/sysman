<script setup>
import Icon from './Icon.vue'

const stores = [
  { name: 'Crash log', path: '~/.local/share/sysman/log/', note: 'JSON Lines, synced every tick, today and yesterday', dot: 'bg-warn' },
  { name: 'History', path: '~/.local/share/sysman/history.db', note: 'SQLite with rollups and recorded events, opened read-only by readers', dot: 'bg-accent' },
  { name: 'Live socket', path: '$XDG_RUNTIME_DIR/sysman.sock', note: 'line-delimited JSON: meta, latest, subscribe, processes', dot: 'bg-ok' },
]

const facts = [
  {
    icon: 'home',
    title: 'Nothing leaves the machine',
    text: 'Live data travels over a Unix socket, history sits in a local file and logs are read where they are. No account, no telemetry, no network connections.',
  },
  {
    icon: 'lock',
    title: 'No root for the essentials',
    text: 'The collector and the app run as you. Values that need root show as “not permitted”, never as a misleading zero.',
  },
  {
    icon: 'terminal',
    title: 'Your data, in open formats',
    text: 'Query the socket with socat and jq, open the database with any SQLite tool, read the crash log with tail.',
  },
]
</script>

<template>
  <section id="architecture" class="relative py-24 sm:py-32">
    <div class="wrap">
      <div class="max-w-2xl">
        <p v-reveal class="eyebrow">How it works</p>
        <h2 v-reveal="60" class="mt-3 text-3xl font-semibold tracking-tight text-balance sm:text-5xl">Three small programs, one job each.</h2>
        <p v-reveal="120" class="mt-5 text-lg leading-relaxed text-muted">
          A collector that records, an app that shows, and an optional helper for the few readings Linux
          keeps behind root.
        </p>
      </div>

      <div v-reveal class="mt-14 grid items-stretch gap-0 lg:grid-cols-[1fr_auto_1.35fr_auto_1fr]">
        <!-- helper -->
        <div class="card flex flex-col p-6">
          <div class="flex items-center gap-3">
            <span class="flex h-9 w-9 items-center justify-center rounded-lg bg-violet/12 text-violet"><Icon name="shield" :size="18" /></span>
            <div>
              <p class="font-mono text-[0.95rem] font-semibold">sysman-helper</p>
              <p class="text-xs text-subtle">optional · system service · off by default</p>
            </div>
          </div>
          <p class="mt-4 text-sm leading-relaxed text-muted">
            Reads only what needs root: SMART drive health, CPU package power, other users’ disk I/O and
            open files, and per-process network traffic through eBPF.
          </p>
          <p class="mt-auto pt-4 text-xs text-subtle">Confined by systemd, with no network of its own.</p>
        </div>

        <div class="flow" aria-hidden="true"><span class="flow-label">helper.sock</span></div>

        <!-- daemon -->
        <div class="card relative flex flex-col overflow-hidden p-6 ring-1 ring-accent/25">
          <div class="pointer-events-none absolute -top-24 left-1/2 h-48 w-72 -translate-x-1/2 rounded-full bg-accent/15 blur-3xl" aria-hidden="true" />
          <div class="relative flex items-center gap-3">
            <span class="flex h-9 w-9 items-center justify-center rounded-lg bg-accent/12 text-accent"><Icon name="cpu" :size="18" /></span>
            <div>
              <p class="font-mono text-[0.95rem] font-semibold">sysmand</p>
              <p class="text-xs text-subtle">collector · systemd user service</p>
            </div>
          </div>
          <p class="relative mt-4 text-sm leading-relaxed text-muted">
            Reads <span class="inline-code">/proc</span>, <span class="inline-code">/sys</span>, NVML and hwmon on a
            fixed tick, each module on its own thread, and writes three things:
          </p>
          <ul class="relative mt-4 space-y-2">
            <li v-for="s in stores" :key="s.name" class="rounded-lg border border-line bg-canvas-2/70 px-3.5 py-2.5">
              <p class="flex items-center gap-2 text-sm font-medium"><span class="h-1.5 w-1.5 rounded-full" :class="s.dot" />{{ s.name }}</p>
              <p class="mt-0.5 font-mono text-[0.72rem] break-all text-muted">{{ s.path }}</p>
              <p class="mt-0.5 text-xs text-subtle">{{ s.note }}</p>
            </li>
          </ul>
        </div>

        <div class="flow" aria-hidden="true"><span class="flow-label">socket + SQLite</span></div>

        <!-- app -->
        <div class="card flex flex-col p-6">
          <div class="flex items-center gap-3">
            <span class="flex h-9 w-9 items-center justify-center rounded-lg bg-ok/12 text-ok"><Icon name="monitor" :size="18" /></span>
            <div>
              <p class="font-mono text-[0.95rem] font-semibold">sysman</p>
              <p class="text-xs text-subtle">desktop app · GPUI</p>
            </div>
          </div>
          <p class="mt-4 text-sm leading-relaxed text-muted">
            Subscribes to the socket for live values and reads history straight from the database. Close it
            any time; recording does not depend on it.
          </p>
          <p class="mt-auto pt-4 text-xs text-subtle">Wayland or X11, with Vulkan.</p>
        </div>
      </div>

      <div class="mt-16 grid gap-8 sm:grid-cols-3">
        <div v-for="(f, i) in facts" :key="f.title" v-reveal="i * 70">
          <span class="flex h-9 w-9 items-center justify-center rounded-lg border border-line bg-surface text-ink"><Icon :name="f.icon" :size="18" /></span>
          <h3 class="mt-4 font-semibold">{{ f.title }}</h3>
          <p class="mt-1.5 text-sm leading-relaxed text-muted">{{ f.text }}</p>
        </div>
      </div>
    </div>
  </section>
</template>

<style scoped>
.flow {
  position: relative;
  display: flex;
  align-items: center;
  justify-content: center;
  height: 4.5rem;
}
.flow::before {
  content: '';
  position: absolute;
  left: 50%;
  top: 0;
  bottom: 0;
  width: 2px;
  transform: translateX(-50%);
  background: repeating-linear-gradient(to bottom, var(--blue) 0 6px, transparent 6px 12px);
  background-size: 2px 12px;
  opacity: 0.55;
  animation: flow-y 0.9s linear infinite;
}
.flow-label {
  position: relative;
  padding: 0.15rem 0.55rem;
  border-radius: 999px;
  border: 1px solid var(--line-strong);
  background: var(--canvas);
  font-family: var(--font-mono);
  font-size: 0.68rem;
  color: var(--muted);
  white-space: nowrap;
}
@media (min-width: 1024px) {
  .flow {
    height: auto;
    width: 6.5rem;
  }
  .flow::before {
    left: 0;
    right: 0;
    top: 50%;
    bottom: auto;
    width: auto;
    height: 2px;
    transform: translateY(-50%);
    background: repeating-linear-gradient(to right, var(--blue) 0 6px, transparent 6px 12px);
    background-size: 12px 2px;
    animation-name: flow-x;
  }
  .flow-label {
    white-space: normal;
    text-align: center;
    max-width: 5.8rem;
  }
}
@keyframes flow-y {
  to {
    background-position: 0 12px;
  }
}
@keyframes flow-x {
  to {
    background-position: 12px 0;
  }
}
</style>
