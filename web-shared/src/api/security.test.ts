import { afterEach, describe, expect, it, vi } from "vitest";

import { setAuthChallengeHandler } from "../api-fetch";
import * as key from "../api-key";
import * as security from "./security";

interface Call {
  url: string;
  method: string;
  headers: Record<string, string>;
}

/** Stand up `fetch` answering one response, recording each call. */
function stubFetch(status: number, body: unknown): Call[] {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      calls.push({
        url,
        method: init.method ?? "GET",
        headers: (init.headers ?? {}) as Record<string, string>,
      });
      return new Response(body === undefined ? "" : JSON.stringify(body), { status });
    }),
  );
  return calls;
}

/** Stand up `fetch` answering by request: the clear succeeds, and the stored-key
 *  check answers `checkStatus`. */
function stubClear(checkStatus: number): Call[] {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      calls.push({ url, method: init.method ?? "GET", headers: (init.headers ?? {}) as Record<string, string> });
      if (url === security.KEY_CHECK_PATH) return new Response("{}", { status: checkStatus });
      return new Response(JSON.stringify({ ok: true }), { status: 200 });
    }),
  );
  return calls;
}

/** Seed this browser's stored key (`setApiKey` also resets the memoised read). */
function seed(value: string | null): void {
  key.setApiKey(value);
}

afterEach(() => {
  vi.unstubAllGlobals();
  setAuthChallengeHandler(null);
});

describe("rotateApiKey", () => {
  it("stores the new key so this browser's next request carries it", async () => {
    seed("old-operator-key");
    const calls = stubFetch(200, { ok: true, api_key: "new-key-123", rotated_at: "2026-11-03T10:00:00Z" });

    const res = await security.rotateApiKey();

    expect(calls[0]).toMatchObject({ url: "/api/v1/security/api-key/rotate", method: "POST" });
    // The rotation itself was authorized with the key held before it.
    expect(calls[0].headers["X-ADOS-Key"]).toBe("old-operator-key");
    expect(res).toEqual({ ok: true, api_key: "new-key-123", rotated_at: "2026-11-03T10:00:00Z" });
    expect(key.getApiKey()).toBe("new-key-123");
  });

  it("stores a key even when the browser held none before", async () => {
    seed(null);
    stubFetch(200, { ok: true, api_key: "fresh", rotated_at: "2026-11-03T10:00:00Z" });
    await security.rotateApiKey();
    expect(key.getApiKey()).toBe("fresh");
  });

  it("keeps the stored key when the agent refuses the rotation", async () => {
    seed("kept");
    stubFetch(403, { detail: "Not over a relay" });
    await expect(security.rotateApiKey()).rejects.toThrow();
    expect(key.getApiKey()).toBe("kept");
  });

  it("keeps the stored key when the answer carries no key", async () => {
    seed("kept");
    stubFetch(200, { ok: true });
    await expect(security.rotateApiKey()).rejects.toThrow();
    expect(key.getApiKey()).toBe("kept");
  });
});

describe("clearApiKey", () => {
  it("drops the panel's own key and raises the access gate when the node refuses it", async () => {
    seed("ados_operator");
    const gate = vi.fn();
    setAuthChallengeHandler(gate);
    const calls = stubClear(401);

    await expect(security.clearApiKey()).resolves.toEqual({ ok: true, droppedStoredKey: true });

    expect(calls.map((c) => `${c.method} ${c.url}`)).toEqual(["DELETE /api/v1/security/api-key", "GET /api/status"]);
    // The check sends the stored key alone, so a live session cannot mask it.
    expect(calls[1].headers).toEqual({ Accept: "application/json", "X-ADOS-Key": "ados_operator" });
    expect(key.getApiKey()).toBeNull();
    expect(gate).toHaveBeenCalledWith(401);
  });

  it("keeps a key the node still accepts (the pairing key) and leaves the gate alone", async () => {
    seed("ados_pairing");
    const gate = vi.fn();
    setAuthChallengeHandler(gate);
    stubClear(200);

    await expect(security.clearApiKey()).resolves.toEqual({ ok: true, droppedStoredKey: false });
    expect(key.getApiKey()).toBe("ados_pairing");
    expect(gate).not.toHaveBeenCalled();
  });

  it("forgets the rotated key once it is cleared", async () => {
    seed(null);
    stubFetch(200, { ok: true, api_key: "ados_rotated", rotated_at: "" });
    await security.rotateApiKey();
    expect(security.rotatedOperatorKey()).toBe("ados_rotated");
    stubClear(401);
    await security.clearApiKey();
    expect(security.rotatedOperatorKey()).toBeNull();
  });

  it("checks nothing when the browser holds no key", async () => {
    seed(null);
    const calls = stubClear(401);
    await expect(security.clearApiKey()).resolves.toEqual({ ok: true, droppedStoredKey: false });
    expect(calls).toHaveLength(1);
  });

  it("keeps the key and the gate down when the clear itself is refused", async () => {
    seed("kept");
    const calls = stubFetch(403, { detail: "Not over a relay" });
    await expect(security.clearApiKey()).rejects.toThrow();
    expect(calls).toHaveLength(1);
    expect(key.getApiKey()).toBe("kept");
  });
});

describe("clearDashboardPin", () => {
  it("sends the contract verb", async () => {
    seed(null);
    const calls = stubFetch(200, { ok: true });
    await expect(security.clearDashboardPin()).resolves.toEqual({ ok: true });
    expect(calls.map((c) => `${c.method} ${c.url}`)).toEqual(["POST /api/dashboard/pin/clear"]);
  });
});
