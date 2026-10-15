<script setup>
import { onBeforeUnmount, onMounted, ref } from 'vue'
import Icon from './Icon.vue'

const options = [
  { id: 'system', label: 'System', icon: 'monitor' },
  { id: 'light', label: 'Light', icon: 'sun' },
  { id: 'dark', label: 'Dark', icon: 'moon' },
]

const mode = ref('system')
const query = typeof matchMedia === 'function' ? matchMedia('(prefers-color-scheme: light)') : null

function apply() {
  const light = mode.value === 'light' || (mode.value === 'system' && query?.matches)
  const theme = light ? 'light' : 'dark'
  const root = document.documentElement
  root.dataset.theme = theme
  root.style.colorScheme = theme
  document.querySelector('meta[name="color-scheme"]')?.setAttribute('content', theme)
  document.querySelector('meta[name="theme-color"]')?.setAttribute('content', light ? '#f6f7fa' : '#14151b')
}

function choose(id) {
  mode.value = id
  try {
    if (id === 'system') localStorage.removeItem('theme')
    else localStorage.setItem('theme', id)
  } catch {}
  apply()
}

function onKey(e, i) {
  const step = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 }[e.key]
  if (!step) return
  e.preventDefault()
  const next = options[(i + step + options.length) % options.length]
  choose(next.id)
  document.getElementById(`theme-${next.id}`)?.focus()
}

onMounted(() => {
  try {
    const stored = localStorage.getItem('theme')
    if (stored === 'light' || stored === 'dark') mode.value = stored
  } catch {}
  query?.addEventListener('change', apply)
})
onBeforeUnmount(() => query?.removeEventListener('change', apply))
</script>

<template>
  <div role="group" aria-label="Appearance" class="flex items-center rounded-lg border border-line p-0.5">
    <button
      v-for="(o, i) in options"
      :id="`theme-${o.id}`"
      :key="o.id"
      type="button"
      :aria-pressed="mode === o.id"
      :aria-label="o.label"
      :title="o.label"
      :tabindex="mode === o.id ? 0 : -1"
      class="inline-flex h-7 w-7 items-center justify-center rounded-md transition"
      :class="mode === o.id ? 'bg-line-strong text-ink' : 'text-subtle hover:text-ink'"
      @click="choose(o.id)"
      @keydown="onKey($event, i)"
    >
      <Icon :name="o.icon" :size="15" />
    </button>
  </div>
</template>
