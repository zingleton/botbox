import { defineConfig } from "vite";

// Tauri-aware Vite config. The dev server port is fixed so it matches
// `devUrl` in tauri.conf.json; HMR/websocket settings keep Tauri happy.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  // The frontend sources (including index.html) live in src/.
  root: "src",
  // Static assets served from the project-root public/ dir, if any.
  publicDir: "../public",
  // Prevent Vite from obscuring Rust errors.
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? { protocol: "ws", host, port: 1421 }
      : undefined,
    watch: {
      // Don't watch the Rust backend; Tauri handles that.
      ignored: ["**/src-tauri/**"],
    },
  },
  // Produce assets the strict CSP can load: no inline scripts/styles.
  build: {
    // Output to project-root dist/ (matches tauri.conf.json frontendDist "../dist").
    outDir: "../dist",
    emptyOutDir: true,
    target: "es2021",
    // Tauri uses Chromium on Windows / WebKit on macOS; modern output is fine.
    minify: !process.env.TAURI_DEBUG ? "esbuild" : false,
    sourcemap: !!process.env.TAURI_DEBUG,
    // Keep CSS in external files so the strict (no unsafe-inline) CSP applies.
    cssCodeSplit: true,
  },
  test: {
    // Vitest's root is the project root regardless of Vite's `root: "src"`,
    // so the glob is relative to the repo root.
    root: ".",
    environment: "jsdom",
    include: ["src/**/*.test.ts"],
    setupFiles: ["./src/test-setup.ts"],
    globals: true,
  },
});
