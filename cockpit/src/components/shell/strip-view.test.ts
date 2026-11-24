import { describe, expect, it } from "vitest";
import type { GsStatus } from "@/lib/types";
import { linkMeasured, stripPaired } from "./strip-view";

const link = (over: Partial<GsStatus["link"]>) => ({ packets_received: 100, link_diag: "healthy", ...over }) as GsStatus["link"];

describe("strip view", () => {
  it("treats a deaf or silent radio as unmeasured", () => {
    expect(linkMeasured(link({}))).toBe(true);
    expect(linkMeasured(link({ packets_received: 0, link_diag: null }))).toBe(false);
    expect(linkMeasured(link({ link_diag: "deaf" }))).toBe(false);
    expect(linkMeasured(link({ link_diag: "no_rf" }))).toBe(false);
    expect(linkMeasured(undefined)).toBe(false);
  });

  it("reads paired from the key fingerprint when the drone id is unknown", () => {
    const status = { paired_drone: { device_id: null, key_fingerprint: "68e0e30ced307f4e" } } as unknown as GsStatus;
    expect(stripPaired(status)).toBe(true);
    expect(stripPaired({ paired_drone: { device_id: null, key_fingerprint: null } } as unknown as GsStatus)).toBe(false);
  });
});
