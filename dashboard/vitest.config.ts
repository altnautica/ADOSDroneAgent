import path from "node:path";
import { defineConfig } from "vitest/config";

// Unit tests for the dashboard's pure logic. Mirrors the cockpit's setup: these
// are DOM-free pure functions, so the node environment is enough — no jsdom.
// The @ alias mirrors the app.
export default defineConfig({
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
