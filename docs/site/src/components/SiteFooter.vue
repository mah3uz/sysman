<script setup>
import Icon from './Icon.vue'
import banner from '@repo/assets/banner.svg'
import logo from '@repo/assets/logo.svg'
</script>

<template>
  <section class="relative overflow-hidden py-20 sm:py-28">
    <div class="wrap">
      <div v-reveal class="relative mx-auto max-w-5xl">
        <div class="pointer-events-none absolute -inset-10 -z-10 rounded-[3rem] bg-accent/10 blur-3xl" aria-hidden="true" />
        <img
          :src="banner"
          width="1600"
          height="520"
          loading="lazy"
          alt="sysman: a system monitor for Linux, live, recorded and crash-proof. CPU, memory, GPU, disk, network and sensors."
          class="h-auto w-full rounded-[1.25rem] shadow-[var(--shot-shadow)] sm:rounded-[1.75rem]"
        />
      </div>
      <div v-reveal="80" class="mt-12 flex flex-wrap items-center justify-center gap-3">
        <a
          href="#install"
          class="inline-flex h-11 items-center gap-2 rounded-xl bg-accent-solid px-5 font-medium text-white shadow-[0_8px_30px_-8px_rgb(10_132_255/0.7),0_0_0_1px_rgb(255_255_255/0.14)_inset] transition hover:brightness-110"
        >
          Install sysman <Icon name="arrow" :size="17" />
        </a>
        <a
          href="https://github.com/mah3uz/sysman"
          class="inline-flex h-11 items-center gap-2 rounded-xl border border-line-strong bg-surface px-5 font-medium transition hover:bg-raised"
        >
          <Icon name="github" :size="18" /> Star on GitHub
        </a>
      </div>
    </div>
  </section>

  <footer class="border-t border-line">
    <div class="wrap flex flex-col gap-6 py-10 text-sm text-muted sm:flex-row sm:items-center sm:justify-between">
      <div class="flex items-center gap-3">
        <img :src="logo" alt="" width="28" height="28" class="h-7 w-7" />
        <p>
          <span class="font-semibold text-ink">sysman</span> by Mahfuz Shaikh ·
          <a href="https://github.com/mah3uz/sysman/blob/main/LICENSE" class="underline-offset-4 hover:text-ink hover:underline">MIT license</a>
        </p>
      </div>
      <ul class="flex flex-wrap gap-x-5 gap-y-2">
        <li><a href="https://github.com/mah3uz/sysman" class="inline-flex items-center gap-1.5 hover:text-ink"><Icon name="github" :size="16" /> GitHub</a></li>
        <li><a href="https://github.com/mah3uz/sysman/releases/latest" class="hover:text-ink">Releases</a></li>
        <li><a href="https://github.com/mah3uz/sysman/issues" class="hover:text-ink">Issues</a></li>
        <li><a href="https://github.com/mah3uz/sysman/blob/main/DESIGN.md" class="hover:text-ink">Design notes</a></li>
      </ul>
    </div>
  </footer>
</template>
