import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Tauri's recommended vite settings — fixed port (1420) so the
// Tauri Rust process knows where the dev server is, no auto-clear
// so cargo errors stay visible, and a `TAURI_DEV_HOST` escape hatch
// for the remote-mobile dev workflow (unused on desktop).
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? { protocol: "ws", host, port: 1421 }
      : undefined,
    watch: {
      // Don't trigger HMR on Rust-side file changes; cargo handles those.
      ignored: ["**/src-tauri/**"],
    },
  },
});
