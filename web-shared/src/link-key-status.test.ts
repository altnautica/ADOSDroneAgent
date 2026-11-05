import { describe, expect, it } from "vitest";

import en from "./locales/en.json";
import { linkKeyStatusView } from "./link-key-status";

function lookup(key: string): unknown {
  return key.split(".").reduce<unknown>(
    (node, part) => (node && typeof node === "object" ? (node as Record<string, unknown>)[part] : undefined),
    en,
  );
}

describe("linkKeyStatusView", () => {
  it("maps each reported state to copy and a tone", () => {
    expect(linkKeyStatusView({ hopping: { key_status: "bound" } })).toEqual({
      labelKey: "wfb.keyStatus.bound",
      variant: "neutral",
    });
    expect(linkKeyStatusView({ hopping: { key_status: "cold_start" } })?.variant).toBe("warn");
    expect(linkKeyStatusView({ hopping: { key_status: "key_unavailable" } })?.variant).toBe("warn");
  });

  it("every state has English copy", () => {
    for (const s of ["bound", "cold_start", "key_unavailable"]) {
      const view = linkKeyStatusView({ hopping: { key_status: s } });
      expect(typeof lookup(view!.labelKey), s).toBe("string");
    }
    expect(typeof lookup("wfb.fields.linkKey")).toBe("string");
  });

  it("is hidden when the state is absent, null or unknown", () => {
    expect(linkKeyStatusView(undefined)).toBeNull();
    expect(linkKeyStatusView({})).toBeNull();
    expect(linkKeyStatusView({ hopping: { enabled: true } })).toBeNull();
    expect(linkKeyStatusView({ hopping: { key_status: null } })).toBeNull();
    expect(linkKeyStatusView({ hopping: { key_status: "toString" } })).toBeNull();
  });
});
