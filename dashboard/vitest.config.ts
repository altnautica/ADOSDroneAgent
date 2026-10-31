import path from "node:path";
import { defineConfig } from "vitest/config";

// Unit tests for the dashboard's pure logic, plus the shared web code's own
// suites. These are DOM-free pure functions, so the node environment is enough
// — no jsdom. The @ alias matches the app.
//
// web-shared/src/*.test.ts runs here, once, from its canonical location:
// scripts/sync-web-shared.sh does not copy test files into the apps. The
// shared files sit outside this package, so their bare imports (react) are
// resolved from the dashboard's node_modules.
export default defineConfig({
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
    dedupe: ["react"],
  },
  server: {
    fs: { allow: [__dirname, path.resolve(__dirname, "../web-shared")] },
  },
  test: {
    environment: "node",
    dir: path.resolve(__dirname, ".."),
    include: ["dashboard/src/**/*.test.ts", "web-shared/src/**/*.test.ts"],
  },
});
