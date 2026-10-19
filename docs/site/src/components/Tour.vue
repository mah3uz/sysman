<script setup>
import { computed, ref } from 'vue'

const screens = [
  {
    id: 'dashboard',
    label: 'Dashboard',
    title: 'The whole machine at a glance',
    text: 'CPU, memory pressure, memory composition, swap, thermals, network and disk on one page, each with its history. Every card and chart opens a detail sheet that explains what the number means and how it is calculated. Crashes, OOM kills, failed services, reboots and package changes are marked on the charts, and the rail lists the recent ones.',
    alt: 'The Dashboard: CPU model and uptime, memory cards for pressure, available, app, cached, compressed and swap, a memory pressure chart, a processor chart, and a rail with per-core CPU bars and memory composition.',
  },
  {
    id: 'processes',
    label: 'Processes',
    title: 'Every process, with its own history',
    text: 'A sortable table with over twenty columns to choose from, a hierarchy view and a search filter. The inspector charts one process’s memory, CPU, open files and disk I/O over time, including earlier runs of the same command.',
    alt: 'The Processes screen: CPU usage, per-core, load average and CPU temperature cards above a table of processes sorted by memory, with CPU %, CPU time, threads, network rates, state and PID columns.',
  },
  {
    id: 'gpu',
    label: 'GPU',
    title: 'Every GPU, whoever made it',
    text: 'Utilisation, VRAM, power against its limit, temperature, clocks and video engines for each device, with the PCIe link, performance states, throttle reasons and per-process use. Machines with several GPUs get a device picker.',
    alt: 'The GPU screen with an NVIDIA and an AMD GPU in the device picker, showing utilisation, power, VRAM, temperature, clock and video cards and a 30-minute utilisation chart.',
  },
  {
    id: 'energy',
    label: 'Energy',
    title: 'Where the watts go',
    text: 'Power measured at the CPU and each GPU, drawn as a flow from the wall to the components to the processes using them. On a laptop: battery charge, draw, estimated runtime and health. Wireless accessories such as mice show their charge too.',
    alt: 'The Energy screen: CPU and GPU power cards, an energy flow diagram from AC power through the CPU and two GPUs to the top processes, and a rail with accessories and electrical readings.',
  },
  {
    id: 'network',
    label: 'Network',
    title: 'Traffic, per adapter and per app',
    text: 'Download and upload history, per-app traffic (with the optional helper), adapters with addresses, link speed and Wi-Fi signal, TCP segments and retransmits, and socket counts.',
    alt: 'The Network screen: download and upload rates, a throughput chart, per-app traffic, and TCP and socket charts in the rail.',
  },
  {
    id: 'diskmap',
    label: 'Disk',
    title: 'Drives, health and what fills them',
    text: 'Throughput, IOPS, latency and queue depth per drive, NVMe temperature, btrfs error counters, free space, and SMART health with the optional helper. Disk Map scans a folder into a treemap, with Largest, Oldest and Kinds views and Move to Trash.',
    alt: 'Disk Map scanning /usr: a treemap of folders and files coloured by kind, with Map, Largest, Oldest and Kinds views.',
  },
  {
    id: 'hardware',
    label: 'Hardware',
    title: 'A full inventory of the machine',
    text: 'System, processor topology, graphics, memory, storage, displays, sensors, network, Wi-Fi, Bluetooth, USB, PCI, audio and input devices, searchable, and exportable as a Markdown report or JSON.',
    alt: 'The Hardware screen: an outline of hardware categories on the left and overview cards for the computer, processor, memory and storage.',
  },
  {
    id: 'explorer',
    label: 'Explorer',
    title: 'Any metric, any time range',
    text: 'Pick recorded metrics and processes, including ones that have exited, and chart them side by side with one shared cursor and recorded events marked. Step back in time, zoom, jump to a date, follow live, and export what you see as CSV.',
    alt: 'Explorer: a metric list on the left, a grid of charts for total CPU, pressure, memory composition and network throughput, and values at the cursor on the right.',
  },
  {
    id: 'insights',
    label: 'Insights',
    title: 'What needs attention',
    text: 'Plain-language callouts from recent history: swap climbing, a process hogging the CPU, memory growing steadily, OOM kills, GPU throttling, disks filling up. Plus memory pressure events and the top consumers over a day or a week.',
    alt: 'Insights: a “Nothing needs attention” callout, a pressure events chart for the last two hours, and a memory growth card for a process.',
  },
  {
    id: 'logs',
    label: 'Logs',
    title: 'What happened, and what the machine was doing',
    text: 'Kernel, hardware, services, apps, scheduled jobs, sign-ins, boot and power, crashes and package history on one timeline, with search, filters and live follow. Worth a look picks out crashes, OOM kills, failed units, GPU and disk errors and boots that ended without a shutdown. Open any entry to see the lines around it and what sysman recorded at that moment.',
    alt: 'The Logs screen: a sidebar of log sources, a boot timeline across the top, Worth a look cards for the findings, and the list of log entries.',
  },
]

const active = ref('dashboard')
const current = computed(() => screens.find((s) => s.id === active.value))

function onKey(e, i) {
  const step = { ArrowRight: 1, ArrowLeft: -1 }[e.key]
  if (!step) return
  e.preventDefault()
  const next = screens[(i + step + screens.length) % screens.length]
  active.value = next.id
  document.getElementById(`tab-${next.id}`)?.focus()
}
</script>

<template>
  <section id="tour" class="relative border-y border-line bg-canvas-2/60 py-24 sm:py-32">
    <div class="wrap">
      <div class="mx-auto max-w-2xl text-center">
        <p v-reveal class="eyebrow">Screen tour</p>
        <h2 v-reveal="60" class="mt-3 text-3xl font-semibold tracking-tight text-balance sm:text-5xl">Eleven screens, one native app.</h2>
        <p v-reveal="120" class="mt-5 text-lg leading-relaxed text-muted">
          A native Linux app built with GPUI and drawn on the GPU. Every image here is a real screenshot
          of sysman reading a live collector.
        </p>
      </div>

      <div v-reveal="160" class="mt-12">
        <div class="-mx-4 overflow-x-auto px-4 pb-2 [mask-image:linear-gradient(90deg,transparent,#000_1rem,#000_calc(100%-1.5rem),transparent)] [scrollbar-width:none] sm:mx-0 sm:px-0 lg:[mask-image:none]">
          <div
            role="tablist"
            aria-label="App screens"
            class="mx-auto flex w-max items-center gap-0.5 rounded-full border border-line-strong bg-surface p-1 shadow-sm"
          >
            <button
              v-for="(s, i) in screens"
              :id="`tab-${s.id}`"
              :key="s.id"
              role="tab"
              type="button"
              :aria-selected="active === s.id"
              :aria-controls="`panel-${s.id}`"
              :tabindex="active === s.id ? 0 : -1"
              class="rounded-full px-3.5 py-1.5 text-sm font-medium whitespace-nowrap transition"
              :class="active === s.id ? 'bg-accent-solid text-white shadow' : 'text-muted hover:bg-line hover:text-ink'"
              @click="active = s.id"
              @keydown="onKey($event, i)"
            >
              {{ s.label }}
            </button>
          </div>
        </div>

        <div
          :id="`panel-${current.id}`"
          role="tabpanel"
          :aria-labelledby="`tab-${current.id}`"
          class="mt-10 grid gap-8 lg:grid-cols-[1fr_20rem] lg:items-start xl:grid-cols-[1fr_22rem]"
        >
          <div class="relative">
            <Transition name="swap" mode="out-in">
              <img
                :key="current.id"
                :src="`/shots/${current.id}.webp`"
                :srcset="`/shots/${current.id}-640.webp 640w, /shots/${current.id}.webp 1280w`"
                sizes="(min-width: 1024px) 860px, 100vw"
                width="1280"
                height="720"
                loading="lazy"
                :alt="current.alt"
                class="shot h-auto w-full"
              />
            </Transition>
          </div>
          <Transition name="fade" mode="out-in">
            <div :key="current.id" class="lg:pt-4">
              <h3 class="text-2xl font-semibold tracking-tight">{{ current.title }}</h3>
              <p class="mt-3 leading-relaxed text-muted">{{ current.text }}</p>
            </div>
          </Transition>
        </div>

        <p class="mt-12 text-center text-sm text-subtle">
          Plus <span class="text-ink">Groups</span>: sets of processes you define, such as a browser and its
          helpers, with their combined memory and CPU over time.
        </p>
      </div>
    </div>
  </section>
</template>

<style scoped>
.swap-enter-active,
.swap-leave-active {
  transition:
    opacity 0.25s ease,
    transform 0.25s ease;
}
.swap-enter-from {
  opacity: 0;
  transform: translateY(6px) scale(0.995);
}
.swap-leave-to {
  opacity: 0;
}
.fade-enter-active,
.fade-leave-active {
  transition: opacity 0.2s ease;
}
.fade-enter-from,
.fade-leave-to {
  opacity: 0;
}
</style>
