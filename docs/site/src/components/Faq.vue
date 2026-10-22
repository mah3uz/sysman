<script setup>
import CodeBlock from './CodeBlock.vue'

const config = `[general]
interval = 2            # base tick in whole seconds
fsync = true            # force each crash-log line to disk

[retention]
raw_hours = 48          # full-resolution history
rollup_1m_days = 90     # 1-minute min/avg/max
rollup_15m_days = 730   # 15-minute min/avg/max
events_days = 90        # recorded events

[modules.processes]     # every module also accepts enabled and interval
top_n = 20`

const socket = `echo '{"cmd":"latest"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/sysman.sock | jq .cpu`

const paths = [
  ['Config', '~/.config/sysman/config.toml'],
  ['History', '~/.local/share/sysman/history.db'],
  ['Crash log', '~/.local/share/sysman/log/YYYY-MM-DD.jsonl'],
  ['Socket', '$XDG_RUNTIME_DIR/sysman.sock'],
]
</script>

<template>
  <section id="faq" class="relative border-t border-line py-24 sm:py-32">
    <div class="wrap grid gap-12 lg:grid-cols-[0.8fr_1.6fr] lg:gap-16">
      <div>
        <p v-reveal class="eyebrow">FAQ</p>
        <h2 v-reveal="60" class="mt-3 text-3xl font-semibold tracking-tight sm:text-5xl">Questions, answered.</h2>
        <p v-reveal="120" class="mt-5 leading-relaxed text-muted">
          Something else? Open an issue on
          <a href="https://github.com/mah3uz/sysman/issues" class="font-medium text-accent underline-offset-4 hover:underline">GitHub</a>.
        </p>
      </div>

      <div v-reveal="100" class="divide-y divide-line border-y border-line">
        <details name="faq" class="group py-1" open>
          <summary>Does sysman need root?</summary>
          <div class="answer">
            <p>
              No. The collector runs as a systemd user service and the app runs as you. A few readings are
              root-only in Linux (SMART health, CPU package power on most systems, other users’ disk I/O and
              open files, which of their processes holds a listening port, per-process network traffic); the
              app shows them as “not permitted” until you turn on the optional helper. The Logs screen
              reads the whole system journal if you are in the
              <span class="inline-code">systemd-journal</span> group (or <span class="inline-code">adm</span>,
              or <span class="inline-code">wheel</span> on Arch); otherwise it shows only your own entries.
            </p>
          </div>
        </details>

        <details name="faq" class="group py-1">
          <summary>Does it send anything over the network?</summary>
          <div class="answer">
            <p>
              No. There is no telemetry, no update check and no account. The app talks to the collector
              over a Unix socket and reads history from a local SQLite file; the helper runs with no network
              access at all. Logs are read where they are: only a short summary of each notable event, such
              as a crash or a reboot, goes into history, never the message text. The history database and
              the crash log are readable only by you.
            </p>
          </div>
        </details>

        <details name="faq" class="group py-1">
          <summary>Does it keep recording when the app is closed?</summary>
          <div class="answer">
            <p>
              Yes. Recording is the collector’s job, and it runs from login whether the app is open or not.
              Open the app later and everything since is there, at full resolution for the last 48 hours.
            </p>
          </div>
        </details>

        <details name="faq" class="group py-1">
          <summary>How much does it cost to run?</summary>
          <div class="answer">
            <p>
              About 2% of one core with the default settings. Some Super I/O sensor chips are expensive to
              read, so they are polled every 10 seconds instead of every 2. The crash log grows by roughly
              290 KB a minute and only today’s and yesterday’s files are kept.
            </p>
          </div>
        </details>

        <details name="faq" class="group py-1">
          <summary>My machine froze or lost power. How do I find out why?</summary>
          <div class="answer">
            <p>
              Open Logs. A boot that ended without a shutdown is listed under Worth a look, with the reset
              reason when an AMD processor reports one, sysman’s last readings before the end and that
              boot’s last log entries. Climbing temperatures or falling fan speeds point at cooling;
              readings flat and normal to the very end point at the power supply or cabling; a burst of
              GPU errors first points at the GPU or its driver.
            </p>
            <p class="mt-3">
              To read it by hand, open that day’s crash log and read the last lines before the gap. Every
              start writes a record with the boot id, so a new id after a gap marks the reboot.
            </p>
          </div>
        </details>

        <details name="faq" class="group py-1">
          <summary>Which systems does it run on?</summary>
          <div class="answer">
            <p>
              Linux on x86-64 with systemd, on AMD or Intel processors and NVIDIA, AMD or Intel graphics.
              The app needs a Wayland or X11 session with Vulkan. There are packages for Arch, Debian 12 or
              newer and Ubuntu 22.04 or newer, and an install script for other distributions.
            </p>
            <p class="mt-3">
              Crash reports come from systemd-coredump, which Debian and Ubuntu do not install by default;
              without it, crashes still show up from the kernel’s segfault lines. Package history is read
              from pacman and apt/dpkg; dnf and zypper are not read yet.
            </p>
          </div>
        </details>

        <details name="faq" class="group py-1">
          <summary>Can I change what it collects and how long it keeps it?</summary>
          <div class="answer">
            <p>
              Yes, in an optional <span class="inline-code">config.toml</span>; every setting has a default.
              Each module can be switched off or slowed down, and each history tier has its own retention.
              A misspelt section or key stops the collector with a message naming it, so a typo cannot
              silently switch something off.
            </p>
            <CodeBlock class="mt-4" :code="config" :shell="false" label="~/.config/sysman/config.toml" />
          </div>
        </details>

        <details name="faq" class="group py-1">
          <summary>Can I use the data in my own scripts?</summary>
          <div class="answer">
            <p>
              Yes. The socket speaks line-delimited JSON with five requests:
              <span class="inline-code">meta</span>, <span class="inline-code">latest</span>,
              <span class="inline-code">subscribe</span>, <span class="inline-code">processes</span> and
              <span class="inline-code">listeners</span>.
              History is plain SQLite in WAL mode; open it read-only.
            </p>
            <CodeBlock class="mt-4" :code="socket" />
          </div>
        </details>

        <details name="faq" class="group py-1">
          <summary>Where does it keep its files?</summary>
          <div class="answer">
            <dl class="grid gap-x-6 gap-y-2 text-sm sm:grid-cols-[auto_1fr]">
              <template v-for="[k, v] in paths" :key="k">
                <dt class="text-subtle">{{ k }}</dt>
                <dd class="font-mono text-[0.8rem] break-all text-ink">{{ v }}</dd>
              </template>
            </dl>
            <p class="mt-3">XDG_CONFIG_HOME and XDG_DATA_HOME are honoured.</p>
          </div>
        </details>

        <details name="faq" class="group py-1">
          <summary>Is it free?</summary>
          <div class="answer">
            <p>Yes. sysman is free and open source.</p>
          </div>
        </details>
      </div>
    </div>
  </section>
</template>

<style scoped>
summary {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 1rem;
  padding: 1.1rem 0.25rem;
  cursor: pointer;
  list-style: none;
  font-weight: 550;
  font-size: 1.02rem;
  border-radius: 0.5rem;
}
summary::-webkit-details-marker {
  display: none;
}
summary::after {
  content: '';
  flex-shrink: 0;
  width: 0.6rem;
  height: 0.6rem;
  border-right: 1.8px solid var(--muted);
  border-bottom: 1.8px solid var(--muted);
  transform: translateY(-25%) rotate(45deg);
  transition: transform 0.25s ease;
}
details[open] > summary::after {
  transform: translateY(25%) rotate(-135deg);
}
summary:hover {
  color: var(--blue);
}
.answer {
  padding: 0 0.25rem 1.3rem;
  color: var(--muted);
  line-height: 1.7;
}
details[open] .answer {
  animation: open 0.3s ease;
}
@keyframes open {
  from {
    opacity: 0;
    transform: translateY(-4px);
  }
}
</style>
