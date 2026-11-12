import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { createDirtyRegistry } from "@/lib/dirty-registry";

const read = (f: string) => readFileSync(path.resolve(__dirname, "..", f), "utf8");

describe("unsaved-change guard", () => {
  it("is dirty while any registered draft is", () => {
    const reg = createDirtyRegistry();
    let calls = 0;
    reg.subscribe(() => calls++);
    reg.set("a", true);
    reg.set("b", false);
    expect(reg.anyDirty()).toBe(true);
    reg.set("a", false);
    expect(reg.anyDirty()).toBe(false);
    reg.set("b", true);
    reg.remove("b");
    expect(reg.anyDirty()).toBe(false);
    expect(calls).toBe(4);
  });

  it("covers every settings draft, with one blocker for the whole app", () => {
    for (const f of [
      "components/settings/config-fields.tsx",
      "routes/settings/capture-settings.tsx",
      "routes/settings/advanced-settings.tsx",
      "routes/settings/cellular-settings.tsx",
      "routes/settings/profile-settings.tsx",
      "routes/settings/cloud-settings.tsx",
      "routes/settings/region-settings.tsx",
      "components/telemetry/mavlink-params.tsx",
    ]) {
      expect(read(f), f).toMatch(/useDirtyGuard\(/);
    }
    // Number and text fields each guard their own draft.
    expect(read("components/settings/config-fields.tsx").match(/useDirtyGuard\(/g)?.length).toBeGreaterThanOrEqual(2);
    // react-router supports one blocker at a time: it lives in the shell.
    expect(read("components/layout/app-shell.tsx")).toMatch(/<DirtyGuardHost \/>/);
  });

  it("blocks in-app navigation while dirty and asks before leaving", () => {
    const src = read("hooks/use-dirty-guard.ts");
    expect(src).toMatch(/useBlocker\(/);
    expect(src).toMatch(/blocker\.proceed\(\)/);
    expect(src).toMatch(/blocker\.reset\(\)/);
  });

  it("runs under a data router, which useBlocker needs", () => {
    const main = read("main.tsx");
    expect(main).toMatch(/createBrowserRouter\(/);
    expect(main).not.toMatch(/<BrowserRouter>/);
  });
});
