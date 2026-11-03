import { describe, expect, it } from "vitest";

import { meshViewKind, parseReceiverCombined, parseReceiverRelays, relayShares } from "@/lib/ground-station";

describe("receiver sources", () => {
  it("reads the combined output and keeps nulls null", () => {
    expect(parseReceiverCombined({ fragments_after_dedup: 8000, fec_repaired: 24, output_kbps: 4200, up: true, stale: false })).toEqual({
      fragmentsAfterDedup: 8000,
      fecRepaired: 24,
      outputKbps: 4200,
      up: true,
      stale: false,
    });
    expect(parseReceiverCombined({ fragments_after_dedup: null, fec_repaired: null, output_kbps: null, up: null, stale: true })).toEqual({
      fragmentsAfterDedup: null,
      fecRepaired: null,
      outputKbps: null,
      up: null,
      stale: true,
    });
    expect(parseReceiverCombined(null)).toBeNull();
  });

  it("treats a stale relay list as unknown, not empty", () => {
    expect(parseReceiverRelays({ relays: null, stale: true })).toEqual({ relays: null, stale: true });
    expect(parseReceiverRelays({ relays: [], stale: false })).toEqual({ relays: [], stale: false });
  });

  it("drops malformed relays and ranks by contribution", () => {
    const parsed = parseReceiverRelays({
      relays: [
        { mac: "aa", last_seen_ms: 1, fragments: 100 },
        { mac: "bb", last_seen_ms: 2, fragments: 300 },
        { mac: "", fragments: 9 },
        { fragments: 9 },
        { mac: "cc", last_seen_ms: 3, fragments: null },
      ],
      stale: false,
    });
    const ranked = relayShares(parsed?.relays ?? []);
    expect(ranked.map((r) => r.mac)).toEqual(["bb", "aa", "cc"]);
    expect(ranked[0].share).toBe(0.75);
    expect(ranked[2].share).toBeNull();
  });
});

describe("mesh view", () => {
  const base = { data: null, httpStatus: null, error: null, role: "relay" };

  it("separates no-snapshot from a live snapshot", () => {
    expect(meshViewKind({ ...base, data: {} })).toBe("no-snapshot");
    expect(meshViewKind({ ...base, data: { up: true, neighbors: [] } })).toBe("ok");
  });

  it("reads a 404 or a direct role as not in a mesh", () => {
    expect(meshViewKind({ ...base, httpStatus: 404, error: "not found" })).toBe("not-in-mesh");
    expect(meshViewKind({ ...base, role: "direct", data: { up: true } })).toBe("not-in-mesh");
  });

  it("waits, then reports a failed read", () => {
    expect(meshViewKind(base)).toBe("loading");
    expect(meshViewKind({ ...base, error: "boom", httpStatus: 500 })).toBe("error");
  });
});
