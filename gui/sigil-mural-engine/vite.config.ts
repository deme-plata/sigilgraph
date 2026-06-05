import { defineConfig } from 'vite'

// base:'./' → relative asset URLs so the built dist works from any sub-path
// (quillon.xyz/sigil-mural-engine/, a flux_ui_deploy folder, file://, etc.)
export default defineConfig({
  base: './',
  build: { outDir: 'dist', emptyOutDir: true, target: 'es2020' },
})
