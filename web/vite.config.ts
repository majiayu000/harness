import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import path from "node:path";

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  server: {
    port: 5173,
    proxy: {
      // Every harness-server route the React app actually hits in dev.
      // Keep in sync with crates/harness-server/src/http/http_router.rs.
      // Do NOT proxy /, /overview, /favicon.ico, or /assets/* — those
      // are served by Vite during dev.
      "/api": "http://localhost:9800",
      "/tasks": "http://localhost:9800",
      "/projects": "http://localhost:9800",
      "/rpc": "http://localhost:9800",
      "/health": "http://localhost:9800",
      "/signals": "http://localhost:9800",
      "/webhook": "http://localhost:9800",
      "/auth": "http://localhost:9800",
      "/ws": { target: "ws://localhost:9800", ws: true },
    },
  },
  build: {
    outDir: "dist",
    assetsDir: "assets",
    sourcemap: false,
    rollupOptions: {
      output: {
        entryFileNames: "assets/[name].[hash].js",
        chunkFileNames: "assets/[name].[hash].js",
        assetFileNames: "assets/[name].[hash][extname]",
      },
    },
  },
  test: {
    globals: true,
    environment: "jsdom",
    setupFiles: ["./src/test-setup.ts"],
    css: false,
  },
});
