<script setup>
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from 'vue'

const N = 30
const W = 300
const H = 84
const STEP = W / (N - 1)
const TICK_MS = 2000

let seed = 7
const rand = () => ((seed = (seed * 16807) % 2147483647) / 2147483647)

let level = 0.3
function next() {
  level += (rand() - 0.5) * 0.12 + (0.3 - level) * 0.12
  if (rand() > 0.9) level += 0.25 + rand() * 0.25
  level = Math.min(0.92, Math.max(0.06, level))
  return level
}

const values = ref(Array.from({ length: N + 1 }, next))
const shift = ref(0)
const animating = ref(false)

const pts = computed(() => values.value.map((v, i) => [i * STEP, H - 6 - v * (H - 14)]))

// Catmull-Rom through every reading, so the line is smooth without overshooting much
const line = computed(() => {
  const p = pts.value
  let d = `M${p[0][0]},${p[0][1]}`
  for (let i = 0; i < p.length - 1; i++) {
    const a = p[i - 1] || p[i], b = p[i], c = p[i + 1], e = p[i + 2] || c
    d += ` C${b[0] + (c[0] - a[0]) / 6},${b[1] + (c[1] - a[1]) / 6} ${c[0] - (e[0] - b[0]) / 6},${c[1] - (e[1] - b[1]) / 6} ${c[0]},${c[1]}`
  }
  return d
})
const area = computed(() => `${line.value} L${N * STEP},${H} L0,${H} Z`)
const last = computed(() => pts.value[pts.value.length - 1])

let timer
async function tick() {
  animating.value = false
  shift.value = 0
  values.value = [...values.value.slice(1), next()]
  await nextTick()
  requestAnimationFrame(() => {
    animating.value = true
    shift.value = -STEP
  })
}

onMounted(() => {
  if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) return
  tick()
  timer = setInterval(tick, TICK_MS)
})
onBeforeUnmount(() => clearInterval(timer))
</script>

<template>
  <svg :viewBox="`0 0 ${W} ${H}`" class="block h-auto w-full" aria-hidden="true">
    <defs>
      <linearGradient id="spark-fill" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0" stop-color="#30d158" stop-opacity="0.38" />
        <stop offset="1" stop-color="#30d158" stop-opacity="0.02" />
      </linearGradient>
      <clipPath id="spark-clip"><rect x="0" y="0" :width="W + 5" :height="H" /></clipPath>
    </defs>
    <line x1="0" :x2="W" :y1="H * 0.35" :y2="H * 0.35" stroke="#ff9f0a" stroke-opacity="0.45" stroke-dasharray="4 3" />
    <g clip-path="url(#spark-clip)">
      <g
        :style="{
          transform: `translateX(${shift}px)`,
          transition: animating ? `transform ${TICK_MS}ms linear` : 'none',
        }"
      >
        <path :d="area" fill="url(#spark-fill)" />
        <path :d="line" fill="none" stroke="#30d158" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" />
        <circle :cx="last[0]" :cy="last[1]" r="3.2" fill="#b8ffc9" />
      </g>
    </g>
  </svg>
</template>
