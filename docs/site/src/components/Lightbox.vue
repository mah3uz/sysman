<script setup>
import { computed, ref } from 'vue'
import Icon from './Icon.vue'
import Shot from './Shot.vue'

const props = defineProps({ screens: { type: Array, required: true } })
const active = defineModel({ type: String, required: true })

const dialog = ref(null)
const closeButton = ref(null)
const isOpen = ref(false)
let opener = null

const index = computed(() => props.screens.findIndex((s) => s.id === active.value))
const current = computed(() => props.screens[index.value])
const at = (i) => props.screens[(i + props.screens.length) % props.screens.length]
const neighbours = computed(() => [at(index.value - 1), at(index.value + 1)])

function go(i) {
  active.value = at(i).id
}

function open(from) {
  opener = from
  isOpen.value = true
  document.documentElement.style.overflow = 'hidden'
  dialog.value.showModal()
  closeButton.value.focus()
}

function onClose() {
  isOpen.value = false
  document.documentElement.style.overflow = ''
  opener?.focus({ preventScroll: true })
}

function onKey(e) {
  const moves = { ArrowLeft: index.value - 1, ArrowRight: index.value + 1, Home: 0, End: props.screens.length - 1 }
  if (e.key in moves) {
    e.preventDefault()
    go(moves[e.key])
  } else if (e.key === 'Tab') {
    // showModal keeps focus off the page but still lets Tab reach the browser's own controls
    const stops = [...dialog.value.querySelectorAll('button')]
    const edge = e.shiftKey ? stops[0] : stops.at(-1)
    if (document.activeElement === edge) {
      e.preventDefault()
      ;(e.shiftKey ? stops.at(-1) : stops[0]).focus()
    }
  }
}

function onClick(e) {
  if (!e.target.closest('img, button, .lb-cap')) dialog.value.close()
}

let touch = null
function onTouchStart(e) {
  touch = e.touches.length === 1 ? { x: e.touches[0].clientX, y: e.touches[0].clientY } : null
}
function onTouchEnd(e) {
  if (!touch) return
  const dx = e.changedTouches[0].clientX - touch.x
  const dy = e.changedTouches[0].clientY - touch.y
  touch = null
  if (Math.abs(dx) > 40 && Math.abs(dx) > Math.abs(dy) * 1.5) go(index.value + (dx < 0 ? 1 : -1))
}

defineExpose({ open })
</script>

<template>
  <dialog
    ref="dialog"
    class="lb"
    aria-labelledby="lb-title"
    @close="onClose"
    @keydown="onKey"
    @click="onClick"
    @touchstart.passive="onTouchStart"
    @touchend.passive="onTouchEnd"
  >
    <div class="lb-body">
      <p class="lb-count text-sm font-medium text-muted tabular-nums" aria-live="polite">
        <span class="sr-only">Screen </span>{{ index + 1 }}<span aria-hidden="true"> / </span><span class="sr-only"> of </span>{{ screens.length }}
      </p>
      <button type="button" class="lb-btn lb-prev" aria-label="Previous screen" @click="go(index - 1)"><Icon name="prev" /></button>
      <button type="button" class="lb-btn lb-next" aria-label="Next screen" @click="go(index + 1)"><Icon name="next" /></button>
      <button ref="closeButton" type="button" class="lb-btn lb-close" aria-label="Close" @click="dialog.close()"><Icon name="x" /></button>

      <Transition name="lb-fade">
        <div v-if="isOpen" :key="current.id" class="lb-view">
          <Shot :id="current.id" large :alt="current.alt" class="shot lb-img" />
        </div>
      </Transition>
      <div v-if="isOpen" hidden>
        <Shot v-for="s in neighbours" :id="s.id" :key="s.id" large alt="" />
      </div>

      <div class="lb-cap mx-auto max-w-3xl text-center">
        <h2 id="lb-title" class="text-lg font-semibold tracking-tight text-ink sm:text-xl">{{ current.title }}</h2>
        <p class="mt-1.5 text-sm leading-relaxed text-pretty text-muted sm:text-[0.95rem]">{{ current.text }}</p>
      </div>
    </div>
  </dialog>
</template>

<style scoped>
.lb {
  position: fixed;
  inset: 0;
  width: 100%;
  height: 100dvh;
  max-width: none;
  max-height: none;
  margin: 0;
  padding: 0;
  border: 0;
  color: var(--text);
  background: color-mix(in srgb, var(--canvas) 94%, transparent);
  backdrop-filter: blur(6px);
}
.lb::backdrop {
  background: transparent;
}
.lb[open] {
  animation: lb-in 0.18s ease-out;
}
@keyframes lb-in {
  from {
    opacity: 0;
  }
}

.lb-body {
  display: grid;
  height: 100%;
  padding: 1rem;
  gap: 0.75rem 0.5rem;
  grid-template-columns: auto auto 1fr auto;
  grid-template-rows: auto minmax(0, 1fr) auto minmax(0, 15rem) minmax(0, 1fr);
  grid-template-areas:
    'prev count next close'
    '. . . .'
    'view view view view'
    'cap cap cap cap'
    '. . . .';
}
@media (min-width: 640px) {
  .lb-body {
    padding: 1.25rem 1.5rem 1.5rem;
    gap: 1rem;
    grid-template-columns: 3rem minmax(0, 1fr) 3rem;
    grid-template-rows: auto minmax(0, 1fr) 8.5rem;
    grid-template-areas:
      '. count close'
      'prev view next'
      '. cap .';
  }
}

.lb-count {
  grid-area: count;
  align-self: center;
}
.lb-prev {
  grid-area: prev;
}
.lb-next {
  grid-area: next;
}
.lb-close {
  grid-area: close;
  justify-self: end;
}
@media (min-width: 640px) {
  .lb-count {
    justify-self: center;
  }
  .lb-prev,
  .lb-next {
    align-self: center;
  }
}

.lb-btn {
  display: grid;
  place-items: center;
  width: 2.75rem;
  height: 2.75rem;
  border-radius: 999px;
  border: 1px solid var(--line-strong);
  background: var(--surface);
  color: var(--text);
  transition: background-color 0.15s ease;
}
.lb-btn:hover {
  background: var(--raised);
}
.lb-btn:focus-visible {
  border-radius: 999px;
}

.lb-view {
  grid-area: view;
  min-width: 0;
  min-height: 0;
  display: flex;
  align-items: center;
  justify-content: center;
}
.lb-view :deep(picture) {
  display: contents;
}
.lb-view :deep(.lb-img) {
  width: auto;
  height: auto;
  max-width: 100%;
  max-height: 100%;
}

.lb-cap {
  grid-area: cap;
  min-height: 0;
  overflow-y: auto;
}

.lb-fade-enter-active,
.lb-fade-leave-active {
  transition: opacity 0.16s ease;
}
.lb-fade-enter-from,
.lb-fade-leave-to {
  opacity: 0;
}

@media (prefers-reduced-motion: reduce) {
  .lb[open] {
    animation: none;
  }
  .lb-fade-enter-active,
  .lb-fade-leave-active {
    transition: none;
  }
}
</style>
