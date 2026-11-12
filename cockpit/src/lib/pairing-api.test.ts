import { afterEach, describe, expect, it, vi } from "vitest";

import {
  acceptPairingCode,
  acceptRefusal,
  bindCancellable,
  normalizePairingCode,
  PAIR_ACTION_GUARD,
  unpair,
} from "@/lib/pairing-api";

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

});

/** Answers each fetch in turn: `armed` is the agent's 409 armed refusal. */
function stubSequence(...answers: ("armed" | "ok")[]) {
  const fetchMock = vi.fn(async () => {
    const a = answers.shift() ?? "ok";
    return a === "armed"
      ? new Response(JSON.stringify({ error: "E_ARMED", message: "Vehicle is armed", override: "force" }), {
          status: 409,
          headers: { "content-type": "application/json" },
        })
      : new Response(JSON.stringify({ ok: true }), { status: 200, headers: { "content-type": "application/json" } });
  });
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}
const urls = (m: ReturnType<typeof stubSequence>) => m.mock.calls.map((c) => String((c as unknown[])[0]));

describe("unpair", () => {
  for (const [target, path] of [
    ["cloud", "/api/pairing/unpair"],
    ["rf", "/api/wfb/pair/unpair"],
  ] as const) {
    it(`posts the ${target} unpair without force when the agent allows it`, async () => {
      const fetchMock = stubSequence("ok");
      const confirm = vi.fn(async () => true);
      await expect(unpair(target, confirm, async () => null)).resolves.toBe("done");
      expect(urls(fetchMock)).toEqual([expect.stringMatching(new RegExp(`${path}$`))]);
      expect(confirm).not.toHaveBeenCalled();
    });

    it(`asks before repeating a refused ${target} unpair with force`, async () => {
      const fetchMock = stubSequence("armed", "ok");
      const confirm = vi.fn(async () => true);
      await expect(unpair(target, confirm, async () => null)).resolves.toBe("done");
      expect(confirm).toHaveBeenCalledOnce();
      expect(urls(fetchMock)[1]).toMatch(new RegExp(`${path}\\?force=1$`));
    });

    it(`words a declined ${target} unpair from the arm state`, async () => {
      stubSequence("armed");
      const no = async () => false;
      await expect(unpair(target, no, async () => ({ connected: true, armed: true }))).resolves.toBe("armed");
      stubSequence("armed");
      await expect(unpair(target, no, async () => null)).resolves.toBe("armedUnknown");
      // The agent refused, so a snapshot saying disarmed is out of date.
      stubSequence("armed");
      await expect(unpair(target, no, async () => ({ connected: false, armed: null }))).resolves.toBe("armedUnknown");
    });
  }
});

describe("bind cancel", () => {
  it("offers Cancel while the agent reports a waiting bind, from any tab", () => {
    for (const state of ["opening_tunnel", "waiting_peer"]) {
      expect(bindCancellable({ state })).toBe(true);
    }
    expect(bindCancellable({ phase: "waiting_peer" })).toBe(true);
    for (const state of ["idle", "transferring_keys", "applying_keys", "paired", "failed", "aborted"]) {
      expect(bindCancellable({ state })).toBe(false);
    }
    expect(bindCancellable(null)).toBe(false);
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
