import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Configuración pensada para Tauri: puerto fijo y sin limpiar la consola.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    target: "es2022",
  },
});
