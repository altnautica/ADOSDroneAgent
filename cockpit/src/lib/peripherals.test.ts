import { describe, expect, it } from "vitest";

import { parsePeripherals, sinceParts } from "@/lib/peripherals";

describe("peripherals", () => {
  it("prefers the connected boolean and builds the detail line", () => {
    const [p] = parsePeripherals({
      peripherals: [
        {
          id: "ados.radio",
          display_name: "Radio",
          connected: true,
          state: "error",
          transport: "usb",
          manifest: { kind: "radio", vendor: "Acme", description: "A radio" },
        },
      ],
    });
    expect(p).toMatchObject({ id: "ados.radio", name: "Radio", connected: true, tone: "ok", description: "A radio" });
    expect(p.detail).toEqual(["usb", "radio", "Acme"]);
    expect(p.lastSeen).toBeNull();
  });

  it("falls back to the state word on older agents", () => {
    const rows = parsePeripherals({
      peripherals: [
        { id: "a", state: "ready" },
        { id: "b", state: "failed" },
        { id: "c", connected: false, last_seen: 100 },
        { id: "d" },
        { name: "no id" },
      ],
    });
    expect(rows.map((r) => [r.id, r.connected, r.tone])).toEqual([
      ["a", true, "ok"],
      ["b", false, "err"],
      ["c", false, "warn"],
      ["d", false, "muted"],
    ]);
    expect(rows[2].lastSeen).toBe(100);
    expect(parsePeripherals(null)).toEqual([]);
  });

  it("buckets the time since last seen", () => {
    expect(sinceParts(0, 30_000)).toEqual({ n: 30, unit: "seconds" });
    expect(sinceParts(0, 7_200_000)).toEqual({ n: 2, unit: "hours" });
    expect(sinceParts(0, 3 * 86_400_000)).toEqual({ n: 3, unit: "days" });
  });
});
