let observer

function getObserver() {
  if (observer === undefined) {
    observer =
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
  }
  return observer
}

// an attribute rather than a class, because hydration checks an element's whole class list
export const reveal = {
  getSSRProps: () => ({ 'data-reveal': '' }),
  mounted(el, binding) {
    el.setAttribute('data-reveal', '')
    const io = getObserver()
    if (!io) return el.classList.add('is-visible')
    if (binding.value) el.style.setProperty('--delay', `${binding.value}ms`)
    io.observe(el)
  },
  unmounted(el) {
    observer?.unobserve(el)
  },
}
