import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

/**
 * Everything is bundled. No CDN, no remote fonts, no external asset host — the
 * Content Security Policy in crates/app/tauri.conf.json forbids reaching any of them,
 * and spec §3.1 forbids the egress in the first place.
 */
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: {
    target: "esnext",
    // Tauri ships the bundle locally; a sourcemap costs nothing and makes a crash
    // report from a user readable.
    sourcemap: true,
    assetsInlineLimit: 0,
  },
});
