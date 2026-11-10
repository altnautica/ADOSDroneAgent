import { beforeEach, describe, expect, it, vi } from "vitest";

import { loadOfferedUids, markSetupOffered, SETUP_OFFERED_KEY, setupOfferUid } from "@/lib/aircraft-setup";
import type { SetupStatus, Wizard } from "@/shared/setup-wizard/types";

const mem = new Map<string, string>();
vi.stubGlobal("localStorage", {
  getItem: (k: string) => mem.get(k) ?? null,
  setItem: (k: string, v: string) => void mem.set(k, v),
  removeItem: (k: string) => void mem.delete(k),
});

function wizard(state: Wizard["state"]): Wizard {
  return { id: "ground", state, available: true, blockers: [], completed_at: null, steps: [] };
}

function status(uid: string | null, wizards: Wizard[]): SetupStatus {
  return { uid, firmware: null, firmware_version: null, vehicle_class: null, running: null, wizards, report: null };
}

beforeEach(() => mem.clear());

describe("aircraft setup offer", () => {
  it("offers an identified FC whose ground wizard is not done, once per uid", () => {
    const s = status("fc-1", [wizard("not_started")]);
    expect(setupOfferUid(s, [], false)).toBe("fc-1");
    expect(setupOfferUid(s, ["fc-1"], false)).toBeNull();
    // A different FC on the same device is offered again.
    expect(setupOfferUid(status("fc-2", []), ["fc-1"], false)).toBe("fc-2");
  });

  it("never offers a set-up FC, an unidentified one, or in the air", () => {
    expect(setupOfferUid(status("fc-1", [wizard("done")]), [], false)).toBeNull();
    expect(setupOfferUid(status(null, []), [], false)).toBeNull();
    expect(setupOfferUid(null, [], false)).toBeNull();
    expect(setupOfferUid(status("fc-1", []), [], true)).toBeNull();
  });

  it("remembers the offered uids per device", () => {
    expect(loadOfferedUids()).toEqual([]);
    markSetupOffered("fc-1");
    markSetupOffered("fc-1");
    markSetupOffered("fc-2");
    expect(JSON.parse(mem.get(SETUP_OFFERED_KEY) ?? "[]")).toEqual(["fc-1", "fc-2"]);
    expect(loadOfferedUids()).toEqual(["fc-1", "fc-2"]);
    mem.set(SETUP_OFFERED_KEY, "not json");
    expect(loadOfferedUids()).toEqual([]);
  });
});

describe("aircraft setup offer text", () => {
  it("uses the spec's setup banner strings", async () => {
    const { default: strings } = await import("@/shared/cockpit-spec/setup-strings.json");
    const s = (strings as { strings: Record<string, string> }).strings;
    expect(s["setup.banner.title"]).toBe("Set up this aircraft");
    expect(s["setup.banner.open"]).toBe("Open");
    expect(s["setup.banner.later"]).toBe("Later");
  });
});
