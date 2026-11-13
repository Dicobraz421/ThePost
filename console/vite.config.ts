import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

// Консоль раздаётся ролью admin под /console/ (ADM-15); узел разработчика — docs/DEV.md, адрес — THEPOST_ADMIN.
// Admin API слушает только TLS: сертификат узла проверяется, его CA — в NODE_EXTRA_CA_CERTS (README).
export default defineConfig({
  base: "/console/",
  plugins: [react()],
  build: { outDir: "dist", sourcemap: false },
  server: {
    proxy: { "/api": { target: process.env.THEPOST_ADMIN ?? "https://127.0.0.1:8443", secure: true } },
  },
  test: {
    environment: "jsdom",
    setupFiles: ["src/test-setup.ts"],
    testTimeout: 20000,
    // Один экземпляр react-router у теста и у привязок Refine.
    server: { deps: { inline: [/@refinedev/] } },
  },
});
