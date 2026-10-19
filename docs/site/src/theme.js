import { onMounted, ref } from 'vue'

// null until mounted: the prerendered page cannot know the theme
const theme = ref(null)
let watching = false

export function useTheme() {
  onMounted(() => {
    if (watching) return
    watching = true
    const root = document.documentElement
    const read = () => (theme.value = root.dataset.theme === 'light' ? 'light' : 'dark')
    read()
    new MutationObserver(read).observe(root, { attributes: true, attributeFilter: ['data-theme'] })
  })
  return theme
}
