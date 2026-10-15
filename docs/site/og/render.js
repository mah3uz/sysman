import { fileURLToPath } from 'node:url'
import { chromium } from 'playwright'

const template = new URL('./template.html', import.meta.url).href
const out = fileURLToPath(new URL('../public/og.jpg', import.meta.url))

const browser = await chromium.launch({ headless: true })
try {
  const page = await browser.newPage({ viewport: { width: 1200, height: 630 } })
  await page.goto(template, { waitUntil: 'load' })
  await page.evaluate(() => document.fonts.ready)
  await page.screenshot({ path: out, type: 'jpeg', quality: 90 })
  console.log(`wrote ${out}`)
} finally {
  await browser.close()
}
