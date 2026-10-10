import { afterEach, describe, expect, it, vi } from "vitest";

import { isValidPin, lockMessage, splashMode, verifyDashboardPin } from "./pin";

describe("isValidPin", () => {
  it("accepts the 4-12 digit PINs the agent accepts", () => {
    // A 6-digit PIN set from the cockpit or Mission Control must be enterable.
    expect(isValidPin("1234")).toBe(true);
    expect(isValidPin("123456")).toBe(true);
    expect(isValidPin("123456789012")).toBe(true);
  });

  it("rejects what the agent rejects", () => {
    expect(isValidPin("123")).toBe(false);
    expect(isValidPin("1234567890123")).toBe(false);
    expect(isValidPin("12a4")).toBe(false);
  });
});

describe("splashMode", () => {
  it("never offers to set a PIN when the status read failed", () => {
    expect(splashMode(null)).toBe("unknown");
  });

  it("picks set / enter / locked off a real status", () => {
    expect(splashMode({ pin_set: false, locked: false, locked_until: null })).toBe("set");
    expect(splashMode({ pin_set: true, locked: false, locked_until: null })).toBe("enter");
    expect(splashMode({ pin_set: true, locked: true, locked_until: 10, scope: "node" })).toBe("locked");
  });
});

describe("node-wide PIN lock", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("words the node lock with the minutes left and how to clear it", () => {
    expect(lockMessage("node", 1500)).toBe(
      "Too many wrong PINs on this node; locked for 25 min. Clear it from the node console or with the pairing key.",
    );
  });

  it("maps a 423 E_PIN_LOCKED to a node-scoped lock", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        new Response(JSON.stringify({ error: "E_PIN_LOCKED", scope: "node", retry_after_s: 600 }), {
          status: 423,
        }),
      ),
    );
    const before = Date.now() / 1000;
    const res = await verifyDashboardPin("1234");
    expect(res.ok).toBe(false);
    if (!res.ok && res.kind === "locked") {
      expect(res.scope).toBe("node");
      expect(res.lockedUntil).toBeGreaterThanOrEqual(before + 600);
    } else {
      throw new Error(`expected a lock, got ${JSON.stringify(res)}`);
    }
  });
});
