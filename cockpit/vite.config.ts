import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";
import path from "node:path";

// The cockpit is served by the agent at /cockpit, so its built asset URLs
// are base-prefixed. Local dev points at a real running agent for the API,
// WHEP, and WS surfaces; override the proxy target with
// `ADOS_AGENT=192.168.x.y:8080 npm run dev` when the agent is elsewhere.
export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), "ADOS_");
  const target = `http://${env.ADOS_AGENT ?? "localhost:8080"}`;

  return {
    base: "/cockpit/",
    plugins: [react()],
    resolve: {
      alias: {
        "@": path.resolve(__dirname, "./src"),
      },
    },
    server: {
      host: true,
      port: 5273,
      proxy: {
        "/api": { target, changeOrigin: true, ws: true },
        "/whep": { target, changeOrigin: true },
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
