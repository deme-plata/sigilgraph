import { defineConfig } from 'vite'

// Standalone single-file ESM bundle of the libp2p stack for the static tron wallet
// page (sigil-wallet-tron.html). No React, no HTML entry — one self-contained
// module loaded via <script type="module" src="sigil-tron-p2p.js">.
export default defineConfig({
  define: {
    'process.env.NODE_ENV': JSON.stringify('production'),
    global: 'globalThis',
  },
  build: {
    outDir: './dist-tron-p2p',
    emptyOutDir: true,
    target: 'es2022',
    minify: 'esbuild',
    lib: {
      entry: 'src/tron-p2p-entry.ts',
      formats: ['es'],
      fileName: () => 'sigil-tron-p2p.js',
    },
    rollupOptions: {
      // Bundle EVERYTHING into one file (no externals, no code-split chunks) so the
      // static page can load a single script with no base-path/asset resolution.
      external: [],
      output: { inlineDynamicImports: true },
    },
  },
})
