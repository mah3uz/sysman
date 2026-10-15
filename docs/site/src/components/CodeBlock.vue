<script setup>
import { computed, ref } from 'vue'
import Icon from './Icon.vue'

const props = defineProps({
  code: { type: String, required: true },
  label: { type: String, default: '' },
  shell: { type: Boolean, default: true },
})

const copied = ref(false)
let timer

const lines = computed(() =>
  props.code.split('\n').map((text) => {
    const hash = text.search(/\s{2,}# /)
    return hash === -1 ? { cmd: text, note: '' } : { cmd: text.slice(0, hash), note: text.slice(hash) }
  }),
)

// shell comments are for reading only, so what is copied runs as pasted
const copyText = computed(() =>
  props.shell
    ? lines.value
        .map((l) => l.cmd.trimEnd())
        .filter((l) => l && !l.startsWith('#'))
        .join('\n')
    : props.code,
)

async function copy() {
  try {
    await navigator.clipboard.writeText(copyText.value)
  } catch {
    const ta = document.createElement('textarea')
    ta.value = copyText.value
    ta.setAttribute('readonly', '')
    ta.style.position = 'fixed'
    ta.style.opacity = '0'
    document.body.append(ta)
    ta.select()
    document.execCommand('copy')
    ta.remove()
  }
  copied.value = true
  clearTimeout(timer)
  timer = setTimeout(() => (copied.value = false), 1800)
}
</script>

<template>
  <div class="relative overflow-hidden rounded-xl border border-white/10 bg-code text-[0.8125rem] leading-relaxed text-[#e6e8ef] shadow-lg shadow-black/20">
    <div v-if="label" class="flex items-center gap-2 border-b border-white/[0.07] px-4 py-2 pr-14 text-xs text-[#a3a8b8]">
      <Icon name="terminal" :size="14" class="shrink-0" />
      <span class="truncate font-mono">{{ label }}</span>
    </div>
    <pre class="overflow-x-auto py-3.5 pr-14 pl-4"><code><template v-for="(l, i) in lines" :key="i"><span v-if="l.cmd.trimStart().startsWith('#')" class="text-[#8e94a9]">{{ l.cmd }}</span><template v-else-if="l.cmd"><span v-if="shell" class="select-none text-[#5fb0ff]">$ </span>{{ l.cmd }}</template><span v-if="l.note" class="text-[#8e94a9]">{{ l.note }}</span>{{ i < lines.length - 1 ? '\n' : '' }}</template></code></pre>
    <button
      type="button"
      class="absolute top-1.5 right-1.5 inline-flex h-8 w-8 items-center justify-center rounded-lg border border-white/10 bg-[#1d1f28] text-[#c9cdd9] transition hover:bg-[#2a2d39] hover:text-white focus-visible:text-white"
      :aria-label="copied ? 'Copied' : 'Copy to clipboard'"
      @click="copy"
    >
      <Icon :name="copied ? 'check' : 'copy'" :size="15" :class="copied ? 'text-[#30d158]' : ''" />
    </button>
    <span class="sr-only" aria-live="polite">{{ copied ? 'Copied to clipboard' : '' }}</span>
  </div>
</template>
