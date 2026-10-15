<script setup>
import Icon from './Icon.vue'

const rows = [
  {
    icon: 'cpu',
    name: 'AMD and Intel CPUs',
    how: 'x86-64. AMD through k10temp and zenpower, Intel through coretemp, with hybrid P- and E-cores told apart. Per-core use and frequency, package and per-CCD temperatures, and RAPL power where it is readable.',
    verified: 'Verified on AMD Ryzen 9 7950X3D',
    accent: 'text-ok bg-ok/12',
  },
  {
    icon: 'monitor',
    name: 'NVIDIA GPUs',
    how: 'NVML in-process, switching to nvidia-smi if NVML ever hangs, so a stuck driver is recorded rather than stalling the collector.',
    verified: 'Verified on GeForce RTX 4090',
    accent: 'text-ok bg-ok/12',
  },
  {
    icon: 'monitor',
    name: 'AMD GPUs',
    how: 'amdgpu sysfs and hwmon, with per-process use from DRM fdinfo.',
    verified: 'Verified on a Raphael iGPU',
    accent: 'text-violet bg-violet/12',
  },
  {
    icon: 'monitor',
    name: 'Intel GPUs',
    how: 'i915 and xe sysfs, hwmon, and per-process use from DRM fdinfo.',
    verified: 'Tested against fixtures from kernel documentation',
    accent: 'text-cool bg-cool/12',
    pending: true,
  },
  {
    icon: 'sensor',
    name: 'Sensors and the rest',
    how: 'Every hwmon sensor on the board (temperatures, fans, voltages, power), NVMe drives, btrfs, batteries and the batteries of wireless peripherals.',
    verified: 'Any Linux machine that exposes them',
    accent: 'text-warn bg-warn/12',
  },
]
</script>

<template>
  <section id="hardware" class="relative border-y border-line bg-canvas-2/60 py-24 sm:py-32">
    <div class="wrap grid gap-12 lg:grid-cols-[0.9fr_1.4fr] lg:gap-16">
      <div class="lg:sticky lg:top-28 lg:self-start">
        <p v-reveal class="eyebrow">Hardware</p>
        <h2 v-reveal="60" class="mt-3 text-3xl font-semibold tracking-tight text-balance sm:text-5xl">Vendor-neutral, top to bottom.</h2>
        <p v-reveal="120" class="mt-5 text-lg leading-relaxed text-muted">
          NVIDIA, AMD and Intel graphics, and AMD and Intel processors, all land in the same records and the
          same screens. A machine with GPUs from two vendors shows both.
        </p>
        <p v-reveal="160" class="mt-5 text-sm leading-relaxed text-subtle">
          Backends without hardware on hand are tested against fixture trees built from the kernel
          documentation, and marked unverified on hardware.
        </p>
      </div>

      <ul class="grid gap-3">
        <li v-for="(r, i) in rows" :key="r.name" v-reveal="i * 50" class="card flex gap-4 p-5 sm:p-6">
          <span class="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl" :class="r.accent"><Icon :name="r.icon" /></span>
          <div class="min-w-0">
            <h3 class="font-semibold">{{ r.name }}</h3>
            <p class="mt-1 text-sm leading-relaxed text-muted">{{ r.how }}</p>
            <p class="mt-3 inline-flex items-center gap-1.5 rounded-full border border-line px-2.5 py-0.5 text-xs" :class="r.pending ? 'text-subtle' : 'text-ink'">
              <span class="h-1.5 w-1.5 rounded-full" :class="r.pending ? 'bg-subtle' : 'bg-ok'" />
              {{ r.verified }}
            </p>
          </div>
        </li>
      </ul>
    </div>
  </section>
</template>
