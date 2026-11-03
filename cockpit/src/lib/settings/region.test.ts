import { describe, expect, it, vi } from "vitest";

import { openGate, settleGate } from "@/lib/settings/apply-form";
import {
  OTHER_REGION,
  draftRegion,
  regionCurrent,
  regionDirty,
  regionPayload,
  regionSeed,
  regionValid,
} from "@/lib/settings/region";

describe("regionCurrent / regionSeed", () => {
  it("reads an absent block as unrestricted", () => {
    expect(regionCurrent(undefined)).toEqual({ mode: "unrestricted", region: null });
    expect(regionCurrent({ region: "DE" })).toEqual({ mode: "unrestricted", region: null });
  });

  it("seeds a common region as a choice and any other code in the free field", () => {
    expect(regionSeed(regionCurrent({ mode: "region", region: "de" }))).toEqual({
      mode: "region",
      choice: "DE",
      otherCode: "",
    });
    expect(regionSeed(regionCurrent({ mode: "region", region: "FR" }))).toEqual({
      mode: "region",
      choice: OTHER_REGION,
      otherCode: "FR",
    });
  });
});

describe("region draft", () => {
  const unrestricted = regionCurrent(undefined);

  it("needs a valid two-letter code only when a region is pinned", () => {
    expect(regionValid({ mode: "unrestricted", choice: OTHER_REGION, otherCode: "x" })).toBe(true);
    expect(regionValid({ mode: "region", choice: OTHER_REGION, otherCode: "F" })).toBe(false);
    expect(draftRegion({ mode: "region", choice: OTHER_REGION, otherCode: " fr " })).toBe("FR");
  });

  it("is dirty on a mode change or a different pinned region", () => {
    const pinned = regionCurrent({ mode: "region", region: "US" });
    expect(regionDirty(regionSeed(unrestricted), unrestricted)).toBe(false);
    expect(regionDirty({ mode: "region", choice: "US", otherCode: "" }, unrestricted)).toBe(true);
    expect(regionDirty({ mode: "region", choice: "US", otherCode: "" }, pinned)).toBe(false);
    expect(regionDirty({ mode: "region", choice: "IN", otherCode: "" }, pinned)).toBe(true);
  });

  it("sends the region only when pinned", () => {
    expect(regionPayload({ mode: "unrestricted", choice: "US", otherCode: "" })).toEqual({
      mode: "unrestricted",
      region: null,
    });
    expect(regionPayload({ mode: "region", choice: OTHER_REGION, otherCode: "fr" })).toEqual({
      mode: "region",
      region: "FR",
    });
  });

  it("posts nothing until the slide confirms", () => {
    const post = vi.fn();
    const gate = openGate({ mode: "region" as const, choice: "JP", otherCode: "" });
    const dismissed = settleGate(gate, false);
    if (dismissed) post(regionPayload(dismissed));
    expect(post).not.toHaveBeenCalled();
    const slid = settleGate(gate, true);
    if (slid) post(regionPayload(slid));
    expect(post).toHaveBeenCalledWith({ mode: "region", region: "JP" });
  });
});
