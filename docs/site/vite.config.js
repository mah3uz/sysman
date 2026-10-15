import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import tailwindcss from '@tailwindcss/vite'

export default defineConfig({
  plugins: [vue(), tailwindcss()],
  resolve: {
    alias: { '@repo': fileURLToPath(new URL('../..', import.meta.url)) },
  },
  // the logo and banner are imported from the repository's assets/ directory
  server: { fs: { allow: ['../..'] } },
})
