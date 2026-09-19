/// <reference types="vitest/config" />
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

// https://vite.dev/config/
//
// `VITE_HS_API_PROXY_TARGET` (e.g. `http://127.0.0.1:8080`) proxies the app's real-server calls
// (`/api/v1`, `/_matrix`, `/_synapse`) to a real `hs serve` running elsewhere during
// development, so `npm run dev` can drive an actual running binary without a CORS dance or
// waiting for the embedded-assets swap (see docs/status/16-management-web-interface.md, "The
// embedded build"). In production the app is served *by* `hs serve` at `/admin/`, so `/api/v1`
// is already same-origin and this proxy is unused — it only matters for local iteration and for
// `test:e2e:real` against a server started out-of-process.
const proxyTarget = process.env.VITE_HS_API_PROXY_TARGET;

export default defineConfig({
  base: "/admin/",
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(import.meta.dirname, "./src"),
    },
  },
  server: {
    proxy: proxyTarget
      ? {
          "/api": { target: proxyTarget, changeOrigin: true },
          "/_matrix": { target: proxyTarget, changeOrigin: true },
          "/_synapse": { target: proxyTarget, changeOrigin: true },
        }
      : undefined,
  },
  build: {
    outDir: "dist",
    sourcemap: true,
  },
  test: {
    environment: "jsdom",
    globals: false,
    setupFiles: ["./src/test/setup.ts"],
    css: true,
    exclude: ["node_modules/**", "e2e/**", "storybook-static/**"],
    coverage: {
      provider: "v8",
      reporter: ["text", "html"],
      exclude: ["node_modules/**", "e2e/**", "**/*.stories.tsx", "src/api/schema.d.ts"],
    },
  },
});
