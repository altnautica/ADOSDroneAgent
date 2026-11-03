import { afterEach, describe, expect, it, vi } from "vitest";

import { setApiKey } from "../api-key";
import { clearSession, setSession } from "../session";
import { isValidPin, lockMessage, setDashboardPin, splashMode, verifyDashboardPin } from "./pin";

/** Stand up `fetch` answering one response, recording each request's headers. */
function stubFetch(status: number, body: unknown): Record<string, string>[] {
  const seen: Record<string, string>[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (_url: string, init: RequestInit = {}) => {
      seen.push((init.headers ?? {}) as Record<string, string>);
      return new Response(JSON.stringify(body), { status });
    }),
  );
  return seen;
}

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

describe("setDashboardPin credentials", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    setApiKey(null);
    clearSession();
  });

  it("carries the stored key so a paired node authorizes the set off-device", async () => {
    setApiKey("ados_operator");
    const seen = stubFetch(200, { ok: true, session: "s-new", expires_at: 0 });
    await expect(setDashboardPin("2468")).resolves.toEqual({ ok: true });
    expect(seen[0]["X-ADOS-Key"]).toBe("ados_operator");
  });

  it("carries the live session when the browser was unlocked with the PIN", async () => {
    setApiKey(null);
    setSession("s-live", Date.now() / 1000 + 600);
    const seen = stubFetch(200, { ok: true, session: "s-new", expires_at: 0 });
    await setDashboardPin("2468", "1357");
    expect(seen[0]["X-ADOS-Dashboard-Session"]).toBe("s-live");
    expect(seen[0]["X-ADOS-Key"]).toBeUndefined();
  });

  it("keeps verify credential-free", async () => {
    setApiKey("ados_operator");
    const seen = stubFetch(401, { remaining_attempts: 4 });
    await verifyDashboardPin("1234");
    expect(seen[0]["X-ADOS-Key"]).toBeUndefined();
  });
});
