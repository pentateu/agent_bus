import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Dev: the SPA on 5173 proxies /api to the daemon (4198). The bearer token
// comes from the URL hash and is attached by the SPA itself.
export default defineConfig({
  plugins: [react()],
  // The daemon serves the SPA under /ui/ (ServeDir at ~/.supervisor/ui with a
  // fallback to index.html). Without this base, the built index.html
  // references /assets/... which 404s and renders a blank page.
  base: "/ui/",
  server: {
    port: 5173,
    proxy: {
      "/api": "http://127.0.0.1:4198",
    },
  },
  build: {
    outDir: "dist",
  },
  test: {
    environment: "node",
  },
});
