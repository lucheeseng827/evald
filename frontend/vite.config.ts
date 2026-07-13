import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The console is embedded in the evald binary (rust-embed over `dist/`) and served
// from the router fallback, so assets must be referenced by RELATIVE paths (`base:
// "./"`) — the same bundle then works whether the node serves it at `/` (OSS) or
// behind the fleet-query login flow (EE). Build output is committed so `cargo build`
// stays Node-free.
export default defineConfig({
  plugins: [react()],
  base: "./",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // A stable asset layout keeps the committed diff small across rebuilds.
    rollupOptions: {
      output: {
        entryFileNames: "assets/[name].js",
        chunkFileNames: "assets/[name].js",
        assetFileNames: "assets/[name][extname]",
      },
    },
  },
  server: {
    // `npm run dev` proxies the API to a locally-running `evald serve`.
    proxy: { "/v1": "http://127.0.0.1:4318" },
  },
});
