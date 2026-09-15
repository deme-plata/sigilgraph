import { defineConfig } from 'vite'
import { resolve } from 'node:path'

// Multi-entry: the deployed root serves ONE top-level html (fluxc serve on
// sigilgraph.org answers a directory with the SPA fallback, not its index),
// so the page is `sigil-vm.html` + hashed assets under /assets — additive.
export default defineConfig({
  base: '/',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    target: 'es2020',
    rollupOptions: {
      input: {
        'sigil-vm': resolve(__dirname, 'sigil-vm.html'),
        index: resolve(__dirname, 'index.html'),
      },
      output: {
        entryFileNames: 'assets/sigil-vm-[hash].js',
        chunkFileNames: 'assets/sigil-vm-[hash].js',
        assetFileNames: 'assets/sigil-vm-[hash][extname]',
      },
    },
  },
})
