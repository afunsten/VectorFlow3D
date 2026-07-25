import { defineConfig } from "vite";

// Minimal static dev server for the observer client. The WebSocket endpoint is
// the Rust `sgs serve` process (default ws://127.0.0.1:8787) and is configured
// at runtime via the `?ws=` query param or the HUD field — no proxy needed.
export default defineConfig({
  server: {
    port: 5173,
    host: "127.0.0.1",
  },
});
