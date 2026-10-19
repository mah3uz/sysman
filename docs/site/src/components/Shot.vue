<script setup>
import { computed } from 'vue'
import { useTheme } from '../theme.js'

defineOptions({ inheritAttrs: false })
const props = defineProps({ id: { type: String, required: true }, sizes: { type: String, required: true } })

// Ids whose -light files are in public/shots. A listed id gets its light pair in the light
// theme; any other shows its dark shot in both themes, so a missing file never breaks an image.
const lightShots = new Set([])

const theme = useTheme()
const srcset = (name) => `/shots/${name}-640.webp 640w, /shots/${name}.webp 1280w`
// before the app mounts, the system preference picks, as the theme script does
const media = computed(() => (theme.value ? (theme.value === 'light' ? 'all' : 'not all') : '(prefers-color-scheme: light)'))
</script>

<template>
  <picture class="block">
    <source v-if="lightShots.has(props.id)" :media="media" :srcset="srcset(`${props.id}-light`)" :sizes="props.sizes" />
    <img :src="`/shots/${props.id}.webp`" :srcset="srcset(props.id)" :sizes="props.sizes" width="1280" height="720" v-bind="$attrs" />
  </picture>
</template>
