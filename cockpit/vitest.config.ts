import path from "node:path";
import { defineConfig } from "vitest/config";

// Unit tests for the cockpit's pure logic (the detection-batch decoder, the
// overlay box-scaling math, the skill gating). These are DOM-free pure functions,
// so the node environment is enough — no jsdom. The @ alias mirrors the app.
export default defineConfig({
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
