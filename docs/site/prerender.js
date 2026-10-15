import { readFileSync, rmSync, writeFileSync } from 'node:fs'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'vite'

const here = (p) => fileURLToPath(new URL(p, import.meta.url))
const ssrDir = here('./dist-ssr/')
const page = here('./dist/index.html')
const slot = '<!--app-html-->'

await build({
  logLevel: 'warn',
  build: { ssr: here('./src/entry-server.js'), outDir: ssrDir, emptyOutDir: true },
})

const { render } = await import(pathToFileURL(`${ssrDir}entry-server.js`).href)
const html = readFileSync(page, 'utf8')
if (!html.includes(slot)) throw new Error(`${page} has no ${slot} to fill`)
const app = await render()
// a function replacer, so a "$" in the page is never read as a replacement pattern
writeFileSync(page, html.replace(slot, () => app))
rmSync(ssrDir, { recursive: true })
console.log('pre-rendered dist/index.html')
