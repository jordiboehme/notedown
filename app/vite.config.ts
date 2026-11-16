import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import tailwindcss from "@tailwindcss/vite";

// Tauri runs `npm run dev` and points its webview at devUrl
// (http://localhost:1420), so the port is fixed and strict. clearScreen is off
// so the output of the Tauri CLI stays readable.
//
// build.target: the app runs in the WebKit webview of macOS 13 or later, whose
// Safari is 16. envPrefix: TAURI_ENV_* tells the frontend what it is built
// for. server.watch.ignored: a Cargo build in src-tauri must not reload the
// frontend.
export default defineConfig({
  plugins: [svelte(), tailwindcss()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
  envPrefix: ["VITE_", "TAURI_ENV_*"],
  build: {
    target: "safari16",
  },
});
