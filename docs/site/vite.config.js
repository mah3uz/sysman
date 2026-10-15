import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import tailwindcss from '@tailwindcss/vite'

const installScript = fileURLToPath(new URL('../../dist/install.sh', import.meta.url))
const cargoToml = readFileSync(fileURLToPath(new URL('../../Cargo.toml', import.meta.url)), 'utf8')
const version = cargoToml.match(/^version = "(.+)"$/m)[1]

// dist/install.sh stays the only copy; the site serves it at /install.sh
function installSh() {
  const serve = (req, res, next) => {
    if (req.url?.split('?')[0] !== '/install.sh') return next()
    res.setHeader('Content-Type', 'text/plain; charset=utf-8')
    res.end(readFileSync(installScript))
  }
  return {
    name: 'install-sh',
    configureServer(server) {
      server.middlewares.use(serve)
    },
    configurePreviewServer(server) {
      server.middlewares.use(serve)
    },
    generateBundle() {
      this.emitFile({ type: 'asset', fileName: 'install.sh', source: readFileSync(installScript) })
    },
  }
}

// the workspace version, so a release deploys the site with its own number
function sysmanVersion() {
  return { name: 'sysman-version', transformIndexHtml: (html) => html.replaceAll('%SYSMAN_VERSION%', version) }
}

export default defineConfig({
  plugins: [vue(), tailwindcss(), installSh(), sysmanVersion()],
  resolve: {
    alias: { '@repo': fileURLToPath(new URL('../..', import.meta.url)) },
  },
  // the logo and banner are imported from the repository's assets/ directory
  server: { fs: { allow: ['../..'] } },
})
