import { defineConfig } from "vite";
import { resolve } from "path";
const pages = ["index","kappa","ritual","lindblad","nations","bridge","mcp"];
export default defineConfig({
  base: "/sigil-cosmos-engine/",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      input: Object.fromEntries(
        pages.map((p) => [p, resolve(__dirname, `${p}.html`)])
      ),
    },
  },
});
