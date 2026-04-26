import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  // Use relative asset paths so the bundle works whether served at
  // /configure/, /nzb/configure/, or /nzbhydra/configure/.
  base: "./",
  build: {
    outDir: "../public/configure",
    emptyOutDir: true,
  },
  server: {
    proxy: {
      "/api": "http://localhost:3000",
    },
  },
});
