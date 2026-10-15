import { init as initAnalytics } from '@plausible-analytics/tracker'
import { createApp } from 'vue'
import App from './App.vue'
import './style.css'

initAnalytics({
  domain: 'sysman.asmechanics.com',
  endpoint: 'https://plus.testtlc.com/api/event',
  outboundLinks: true,
})

const observer =
  'IntersectionObserver' in window
    ? new IntersectionObserver(
        (entries) => {
          for (const e of entries) {
            if (e.isIntersecting) {
              e.target.classList.add('is-visible')
              observer.unobserve(e.target)
            }
          }
        },
        { rootMargin: '0px 0px -8% 0px', threshold: 0.08 },
      )
    : null

const reveal = {
  mounted(el, binding) {
    if (!observer) return
    el.classList.add('reveal')
    if (binding.value) el.style.setProperty('--delay', `${binding.value}ms`)
    observer.observe(el)
  },
  unmounted(el) {
    observer?.unobserve(el)
  },
}

createApp(App).directive('reveal', reveal).mount('#app')
