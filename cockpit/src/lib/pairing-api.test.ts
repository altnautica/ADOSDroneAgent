import { afterEach, describe, expect, it, vi } from "vitest";

import { acceptPairingCode, acceptRefusal, normalizePairingCode, PAIR_ACTION_GUARD, unpairCloud } from "@/lib/pairing-api";

afterEach(() => {
  vi.unstubAllGlobals();
});

function stubFetch(body: unknown) {
  const fetchMock = vi.fn(async () => new Response(JSON.stringify(body), { status: 200, headers: { "content-type": "application/json" } }));
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

describe("accept code", () => {
  it("reads a 200 with ok:false as a refusal", () => {
    expect(acceptRefusal({ ok: false, message: "code expired" }, "fallback")).toBe("code expired");
    expect(acceptRefusal({ ok: false, error: "bad code" }, "fallback")).toBe("bad code");
    expect(acceptRefusal({}, "fallback")).toBe("fallback");
    expect(acceptRefusal(null, "fallback")).toBe("fallback");
    expect(acceptRefusal({ ok: true }, "fallback")).toBeNull();
  });

  it("strips whitespace from a typed code and refuses an empty one", () => {
    expect(normalizePairingCode(" 123 456 ")).toBe("123456");
    expect(normalizePairingCode("   ")).toBeNull();
  });

  it("throws the agent's reason when the accept is refused", async () => {
    const fetchMock = stubFetch({ ok: false, message: "code expired" });
    await expect(acceptPairingCode("123456", "fallback")).rejects.toThrow("code expired");
    expect(fetchMock).toHaveBeenCalledOnce();
  });

  it("resolves when the agent pairs", async () => {
    stubFetch({ ok: true });
    await expect(acceptPairingCode("123456", "fallback")).resolves.toBeUndefined();
  });

  it("posts the unpair", async () => {
    const fetchMock = stubFetch({ ok: true });
    await unpairCloud();
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toContain("/api/pairing/unpair");
    expect(init.method).toBe("POST");
  });
});

describe("confirm gate", () => {
  it("guards both unpairs and nothing that is not destructive", () => {
    expect(PAIR_ACTION_GUARD.cloudUnpair).toBe("guarded");
    expect(PAIR_ACTION_GUARD.rfUnpair).toBe("guarded");
    expect(PAIR_ACTION_GUARD.cloudAccept).toBe("none");
    expect(PAIR_ACTION_GUARD.phoneApprove).toBe("none");
  });
});
