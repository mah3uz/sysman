<script setup>
import { computed, ref } from 'vue'
import CodeBlock from './CodeBlock.vue'
import Icon from './Icon.vue'

const RELEASES = 'https://github.com/mah3uz/sysman/releases/latest'

const methods = [
  {
    id: 'arch',
    label: 'Arch Linux',
    intro: 'From the AUR: sysman builds the release from source, sysman-bin installs the prebuilt binaries. Use your AUR helper of choice.',
    code: `yay -S sysman  # or sysman-bin
systemctl --user enable --now sysmand`,
  },
  {
    id: 'debian',
    label: 'Debian / Ubuntu',
    intro: 'Download sysman_<version>-1_amd64.deb from the latest release (Ubuntu 22.04 or newer, Debian 12 or newer) and install it with its dependencies.',
    link: true,
    code: `sudo apt install ./sysman_<version>-1_amd64.deb
systemctl --user enable --now sysmand`,
  },
  {
    id: 'other',
    label: 'Other distros',
    intro: 'The install script downloads the prebuilt x86-64 release from GitHub, verifies its checksums, installs it under /usr/local with sudo and starts the collector for you. Pass --version X.Y.Z for another release, or --uninstall to remove exactly what it installed.',
    code: `curl -fsSL https://sysman.asmechanics.com/install.sh | sh`,
  },
  {
    id: 'source',
    label: 'From source',
    intro: 'Needs a recent stable Rust (edition 2024) and, for the app, a Wayland or X11 session with Vulkan.',
    code: `git clone https://github.com/mah3uz/sysman && cd sysman
cargo install --path crates/sysmand
cp dist/sysmand.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now sysmand
cargo build --release -p sysman
./target/release/sysman`,
  },
]

const active = ref('arch')
const current = computed(() => methods.find((m) => m.id === active.value))

function onKey(e, i) {
  const step = { ArrowRight: 1, ArrowLeft: -1 }[e.key]
  if (!step) return
  e.preventDefault()
  const next = methods[(i + step + methods.length) % methods.length]
  active.value = next.id
  document.getElementById(`install-tab-${next.id}`)?.focus()
}

const check = `systemctl --user status sysmand
sysmand --once  # prints one sample, writes nothing`

const helper = `sudo systemctl enable --now sysman-helper`
</script>

<template>
  <section id="install" class="relative py-24 sm:py-32">
    <div class="wrap">
      <div class="mx-auto max-w-2xl text-center">
        <p v-reveal class="eyebrow">Install</p>
        <h2 v-reveal="60" class="mt-3 text-3xl font-semibold tracking-tight text-balance sm:text-5xl">Up and recording in a minute.</h2>
        <p v-reveal="120" class="mt-5 text-lg leading-relaxed text-muted">
          Packages for Arch and Debian-based systems, a script for everything else. Each one installs the
          collector, the app with its desktop entry, and the helper, which stays off until you turn it on.
        </p>
      </div>

      <div v-reveal="160" class="card mx-auto mt-12 max-w-3xl overflow-hidden">
        <div role="tablist" aria-label="Install method" class="flex overflow-x-auto border-b border-line [scrollbar-width:none]">
          <button
            v-for="(m, i) in methods"
            :id="`install-tab-${m.id}`"
            :key="m.id"
            role="tab"
            type="button"
            :aria-selected="active === m.id"
            aria-controls="install-panel"
            :tabindex="active === m.id ? 0 : -1"
            class="relative shrink-0 px-4 py-3.5 text-sm font-medium whitespace-nowrap transition sm:px-5"
            :class="active === m.id ? 'text-ink' : 'text-muted hover:text-ink'"
            @click="active = m.id"
            @keydown="onKey($event, i)"
          >
            {{ m.label }}
            <span v-if="active === m.id" class="absolute inset-x-3 -bottom-px h-0.5 rounded-full bg-accent" />
          </button>
        </div>
        <div id="install-panel" role="tabpanel" :aria-labelledby="`install-tab-${current.id}`" class="p-5 sm:p-7">
          <p class="leading-relaxed text-muted">
            {{ current.intro }}
            <a v-if="current.link" :href="RELEASES" class="inline-flex items-center gap-1 font-medium text-accent underline-offset-4 hover:underline">
              Latest release <Icon name="arrow" :size="14" />
            </a>
          </p>
          <CodeBlock class="mt-5" :code="current.code" />
          <p class="mt-5 text-sm text-subtle">
            Then open <span class="font-medium text-ink">sysman</span> from your application menu. To check on the collector:
          </p>
          <CodeBlock class="mt-3" :code="check" />
        </div>
      </div>

      <!-- helper -->
      <div id="helper" class="mx-auto mt-24 grid max-w-5xl items-center gap-10 lg:grid-cols-[1fr_1.05fr] [&>*]:min-w-0">
        <div>
          <p v-reveal class="eyebrow">Optional</p>
          <h3 v-reveal="60" class="mt-3 text-2xl font-semibold tracking-tight sm:text-3xl">Turn on the helper for the last few readings.</h3>
          <p v-reveal="120" class="mt-4 leading-relaxed text-muted">
            Without root, Linux hides SMART drive health, CPU package power on most systems, other users’
            disk I/O and open files, per-process network traffic, and which of another user’s processes holds
            a listening port. <span class="font-mono text-[0.92em] text-ink">sysman-helper</span>
            reads exactly these and nothing else. It runs as a confined system service with no network,
            answers only on a socket that administrators can open, and never runs anything a client sends.
          </p>
          <p v-reveal="160" class="mt-4 leading-relaxed text-muted">
            Switch on <span class="font-medium text-ink">Read privileged data</span> in the app’s Settings, under
            Advanced; it asks for authentication. Or from a terminal:
          </p>
          <CodeBlock v-reveal="200" class="mt-4" :code="helper" />
          <p class="mt-4 text-sm text-subtle">
            The socket is open to the <span class="inline-code">wheel</span> group on Arch and to
            <span class="inline-code">sudo</span> on Debian and Ubuntu. Add yourself to it if you are not
            in it, and log in again.
          </p>
        </div>
        <figure v-reveal="120" class="relative">
          <div class="pointer-events-none absolute -inset-x-2 -inset-y-6 -z-10 sm:-inset-6 rounded-[2rem] bg-violet/10 blur-2xl" aria-hidden="true" />
          <img
            src="/shots/helper.webp"
            width="560"
            height="352"
            loading="lazy"
            alt="The app’s Settings, Advanced tab: Read privileged data switched on, the helper running, and drive health, CPU package power, other users’ disk I/O and open files, and per-app network traffic all available."
            class="shot h-auto w-full"
          />
          <figcaption class="mt-3 text-center text-xs text-subtle">Settings → Advanced, with each privileged source checked.</figcaption>
        </figure>
      </div>
    </div>
  </section>
</template>
