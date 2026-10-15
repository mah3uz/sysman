<script setup>
import { onBeforeUnmount, onMounted, ref } from 'vue'
import Icon from './Icon.vue'
import logo from '@repo/assets/logo.svg'

const links = [
  { href: '#features', label: 'Features' },
  { href: '#tour', label: 'Tour' },
  { href: '#architecture', label: 'How it works' },
  { href: '#hardware', label: 'Hardware' },
  { href: '#faq', label: 'FAQ' },
]

const scrolled = ref(false)
const onScroll = () => (scrolled.value = window.scrollY > 8)
onMounted(() => {
  onScroll()
  window.addEventListener('scroll', onScroll, { passive: true })
})
onBeforeUnmount(() => window.removeEventListener('scroll', onScroll))
</script>

<template>
  <a href="#main" class="sr-only z-50 rounded-lg bg-accent-solid px-4 py-2 font-medium text-white focus:not-sr-only focus:fixed focus:top-3 focus:left-3">Skip to content</a>
  <header
    class="sticky top-0 z-40 border-b transition-colors duration-300"
    :class="scrolled ? 'border-line bg-canvas/80 backdrop-blur-xl backdrop-saturate-150' : 'border-transparent'"
  >
    <nav class="wrap flex h-16 items-center gap-6" aria-label="Main">
      <a href="#top" class="flex shrink-0 items-center gap-2.5 font-semibold tracking-tight" aria-label="sysman, back to top">
        <img :src="logo" alt="" width="30" height="30" class="h-[30px] w-[30px]" />
        <span class="text-[1.05rem]">sysman</span>
      </a>
      <ul class="hidden items-center gap-1 text-sm text-muted md:flex">
        <li v-for="l in links" :key="l.href">
          <a :href="l.href" class="rounded-md px-3 py-1.5 transition hover:bg-line hover:text-ink">{{ l.label }}</a>
        </li>
      </ul>
      <div class="ml-auto flex items-center gap-2">
        <a
          href="https://github.com/mah3uz/sysman"
          class="inline-flex h-9 items-center gap-2 rounded-lg px-2.5 text-sm text-muted transition hover:bg-line hover:text-ink"
        >
          <Icon name="github" :size="18" />
          <span class="hidden sm:inline">GitHub</span>
        </a>
        <a
          href="#install"
          class="inline-flex h-9 items-center rounded-lg bg-accent-solid px-3.5 text-sm font-medium text-white shadow-[0_0_0_1px_rgb(255_255_255/0.12)_inset] transition hover:brightness-110"
        >
          Install
        </a>
      </div>
    </nav>
  </header>
</template>
