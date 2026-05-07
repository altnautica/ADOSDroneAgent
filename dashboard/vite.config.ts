import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";
import path from "node:path";

// Local dev points the dashboard at a real running agent. Override the
// proxy target with `ADOS_AGENT=192.168.x.y:8080 npm run dev` when the
// agent is not on `skynode.local`.
export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), "ADOS_");
  const target = `http://${env.ADOS_AGENT ?? "skynode.local:8080"}`;

  return {
    plugins: [react()],
    resolve: {
      alias: {
        "@": path.resolve(__dirname, "./src"),
      },
    },
    server: {
      host: true,
      port: 5173,
      proxy: {
        "/api": { target, changeOrigin: true, ws: true },
        "/whep": { target, changeOrigin: true },
        "/hls": { target, changeOrigin: true },
        "/brand.svg": { target },
      },
    },
    build: {
      outDir: "dist",
      emptyOutDir: true,
      sourcemap: false,
      target: "es2022",
    },
  };
});
