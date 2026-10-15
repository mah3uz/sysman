<script setup>
import Icon from './Icon.vue'
import LiveSpark from './LiveSpark.vue'
</script>

<template>
  <section id="top" class="relative isolate overflow-hidden pt-14 pb-20 sm:pt-20 sm:pb-28">
    <div class="hero-bg pointer-events-none absolute inset-0 -z-10" aria-hidden="true" />
    <div class="wrap text-center">
      <p v-reveal class="mx-auto inline-flex items-center gap-2 rounded-full border border-line bg-surface/70 px-3.5 py-1.5 text-xs font-medium text-muted backdrop-blur">
        <span class="relative flex h-2 w-2">
          <span class="absolute inline-flex h-full w-full animate-ping rounded-full bg-ok opacity-60" />
          <span class="relative inline-flex h-2 w-2 rounded-full bg-ok" />
        </span>
        A system monitor for Linux · free and open source
      </p>
      <h1 v-reveal="60" class="mx-auto mt-7 max-w-4xl text-[2.6rem] leading-[1.04] font-semibold tracking-[-0.035em] text-balance sm:text-6xl lg:text-7xl">
        Live, recorded and
        <span class="grad-text bg-clip-text text-transparent">crash&#8209;proof.</span>
      </h1>
      <p v-reveal="120" class="mx-auto mt-6 max-w-2xl text-base leading-relaxed text-pretty text-muted sm:text-lg">
        sysman samples your CPU, memory, GPUs, disks, network, sensors and processes every two seconds.
        A native app shows it live and as history, and the last readings before a freeze or power cut
        are already on disk.
      </p>
      <div v-reveal="180" class="mt-9 flex flex-wrap items-center justify-center gap-3">
        <a
          href="#install"
          class="inline-flex h-11 items-center gap-2 rounded-xl bg-accent-solid px-5 font-medium text-white shadow-[0_8px_30px_-8px_rgb(10_132_255/0.7),0_0_0_1px_rgb(255_255_255/0.14)_inset] transition hover:brightness-110"
        >
          Install sysman <Icon name="arrow" :size="17" />
        </a>
        <a
          href="https://github.com/mah3uz/sysman"
          class="inline-flex h-11 items-center gap-2 rounded-xl border border-line-strong bg-surface/60 px-5 font-medium backdrop-blur transition hover:bg-surface"
        >
          <Icon name="github" :size="18" /> View source
        </a>
      </div>
      <ul v-reveal="220" class="mt-7 flex flex-wrap justify-center gap-x-6 gap-y-2 text-sm text-subtle">
        <li class="flex items-center gap-1.5"><Icon name="check" :size="15" class="text-ok" /> No root needed</li>
        <li class="flex items-center gap-1.5"><Icon name="check" :size="15" class="text-ok" /> Stays on your machine</li>
        <li class="flex items-center gap-1.5"><Icon name="check" :size="15" class="text-ok" /> About 2% of one core</li>
      </ul>
    </div>

    <div v-reveal="200" class="wrap relative mt-14 sm:mt-16">
      <div class="relative mx-auto max-w-6xl">
        <div class="hero-halo pointer-events-none absolute -inset-x-8 -top-10 -bottom-6 -z-10 rounded-[3rem]" aria-hidden="true" />
        <img
          src="/shots/gpu.webp"
          srcset="/shots/gpu-640.webp 640w, /shots/gpu.webp 1280w"
          sizes="(min-width: 1200px) 1152px, 100vw"
          width="1280"
          height="720"
          fetchpriority="high"
          alt="The sysman GPU screen: utilisation, power, VRAM, temperature, clock and video engine cards above a 30-minute utilisation chart, with device details and per-category GPU use in the side rail."
          class="shot h-auto w-full"
        />
        <div
          class="live-card absolute -bottom-8 left-3 hidden w-[17.5rem] rounded-2xl border border-white/10 bg-[#1b1d26]/90 p-4 text-left text-[#eceef3] shadow-2xl shadow-black/50 backdrop-blur-xl sm:block lg:-left-10 lg:w-[19rem]"
        >
          <div class="flex items-center justify-between text-[0.7rem] font-semibold tracking-[0.08em] uppercase">
            <span class="flex items-center gap-2 text-[#30d158]">
              <span class="relative flex h-2 w-2">
                <span class="absolute inline-flex h-full w-full animate-ping rounded-full bg-[#30d158] opacity-60" />
                <span class="relative inline-flex h-2 w-2 rounded-full bg-[#30d158]" />
              </span>
              Live
            </span>
            <span class="text-[#9ea3b5]">every 2 s</span>
          </div>
          <div class="mt-3"><LiveSpark /></div>
          <p class="mt-2 text-xs text-[#9ea3b5]">Each tick is synced to disk before the next one starts.</p>
        </div>
      </div>
    </div>
  </section>
</template>

<style scoped>
.hero-bg {
  background:
    radial-gradient(60rem 30rem at 50% -8rem, var(--glow), transparent 70%),
    radial-gradient(30rem 20rem at 85% 30%, rgb(48 209 88 / 0.07), transparent 70%);
}
.hero-bg::after {
  content: '';
  position: absolute;
  inset: 0;
  background-image:
    linear-gradient(var(--line) 1px, transparent 1px),
    linear-gradient(90deg, var(--line) 1px, transparent 1px);
  background-size: 56px 56px;
  mask-image: radial-gradient(ellipse 70% 55% at 50% 0%, #000 30%, transparent 75%);
  opacity: 0.55;
}
.grad-text {
  background-image: linear-gradient(90deg, #5bdcef, #2e9bf5 55%, #3d6dff);
}
@media (prefers-color-scheme: light) {
  .grad-text {
    background-image: linear-gradient(90deg, #0a7ea8, #0a63e8 55%, #2f4fe0);
  }
}
.hero-halo {
  background: radial-gradient(closest-side, rgb(10 132 255 / 0.28), transparent);
  filter: blur(30px);
}
.live-card {
  animation: float 7s ease-in-out infinite;
}
@keyframes float {
  50% {
    transform: translateY(-6px);
  }
}
</style>
