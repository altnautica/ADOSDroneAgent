import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { LEASE_OWNER, makeLeaseOwner } from "@/lib/lease-owner";

const src = (rel: string) => readFileSync(path.resolve(__dirname, "..", rel), "utf8");

describe("vision lease owner", () => {
  it("is one id per window, never the shared `cockpit`", () => {
    expect(LEASE_OWNER).toMatch(/^cockpit:[0-9a-f]{8}$/);
    let n = 0;
    const seq = () => ((n += 0.37) % 1);
    const a = makeLeaseOwner(seq);
    const b = makeLeaseOwner(seq);
    expect(a).not.toBe(b);
  });

  it("is what the depth/surfaces and detection demand leases name", () => {
    expect(src("components/feed/layers/layer-stack.tsx")).not.toMatch(/owner:\s*"cockpit"/);
    expect(src("lib/api.ts")).not.toMatch(/owner\s*=\s*"cockpit"/);
  });
});

describe("kiosk paint cap", () => {
  it("comes from the spec's overlay_fps_cap, not a second constant", () => {
    const stack = src("components/feed/layers/layer-stack.tsx");
    expect(stack).not.toMatch(/KIOSK_FPS/);
    expect(src("components/screens/feed-screen.tsx")).toMatch(/fpsCap=\{m\.overlayFpsCap \?\? 0\}/);
  });
});
