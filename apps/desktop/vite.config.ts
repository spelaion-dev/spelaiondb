import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Fixed port: tauri.conf.json points to it in development.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: { target: "es2022", chunkSizeWarningLimit: 2000 },
});
