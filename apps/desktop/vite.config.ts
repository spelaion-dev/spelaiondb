import { readFileSync } from "node:fs";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The development Content Security Policy (SDB-127) is `tauri.conf.json`'s `devCsp`. Tauri adds
// a policy only to the files it serves itself, and in development the window loads this
// server directly: the server sends it, so `tauri dev` runs under a policy too.
const security = JSON.parse(readFileSync(new URL("./src-tauri/tauri.conf.json", import.meta.url), "utf8")).app.security;
const devPolicy = Object.entries(security.devCsp as Record<string, string>)
  .map(([directive, sources]) => `${directive} ${sources}`)
  .join("; ");

// Fixed port: tauri.conf.json points to it in development.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 1420, strictPort: true, headers: { "Content-Security-Policy": devPolicy } },
  build: { target: "es2022", chunkSizeWarningLimit: 2000 },
});
