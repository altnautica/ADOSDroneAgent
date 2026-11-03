import { describe, expect, it } from "vitest";

import { REACQUIRED_WORD_MS, whatsLocked, type WhatsLockedInput } from "@/lib/whats-locked";
import { parseFollowState, type FollowState } from "@/shared/hud/nav-types";

const follow = (raw: Record<string, unknown>): FollowState => parseFollowState(raw)!;

const base: WhatsLockedInput = {
  designated: { cameraId: "main", trackId: 12, bbox: { x: 0, y: 0, width: 10, height: 20 } },
  detection: { classLabel: "person", confidence: 0.974, trackId: 12, lockState: "locked", bbox: { x: 0, y: 0, width: 10, height: 20 } },
  staleReason: null,
  follow: null,
  followAgeMs: 0,
  event: null,
  eventAgeMs: 0,
};

describe("whatsLocked", () => {
  it("names who and the live confidence", () => {
    expect(whatsLocked(base)).toEqual({ who: "PERSON · #12", word: "Locked", tone: "track", confidence: "97%" });
  });

  it("a stale feed wins over every other word", () => {
    const r = whatsLocked({ ...base, staleReason: "No video", follow: follow({ phase: "following" }) });
    expect(r).toMatchObject({ word: "No video", tone: "muted" });
  });

  it("a fresh follow phase wins over the lock words", () => {
    expect(whatsLocked({ ...base, follow: follow({ phase: "following" }) }).word).toBe("Following");
    expect(whatsLocked({ ...base, follow: follow({ phase: "acquiring" }) }).word).toBe("Acquiring");
    expect(whatsLocked({ ...base, follow: follow({ phase: "coasting", coast_remaining_s: 1.43 }) })).toMatchObject({
      word: "Coasting 1.4 s",
      tone: "coast",
    });
    expect(whatsLocked({ ...base, follow: follow({ phase: "searching" }) }).tone).toBe("coast");
    expect(whatsLocked({ ...base, follow: follow({ phase: "holding", hold_reason: "HOLD_NO_RANGE" }) })).toMatchObject({
      word: "Holding · No range",
      tone: "warn",
    });
    expect(whatsLocked({ ...base, follow: follow({ phase: "lost" }) })).toMatchObject({ word: "Lost", tone: "crit" });
  });

  it("ignores a stale or idle follow report", () => {
    expect(whatsLocked({ ...base, follow: follow({ phase: "following" }), followAgeMs: 2001 }).word).toBe("Locked");
    expect(whatsLocked({ ...base, follow: follow({ phase: "idle" }) }).word).toBe("Locked");
  });

  it("shows a re-acquire for 4 s", () => {
    const event = { topic: "follow.reacquired" as const, trackId: 12, similarity: 0.862, gapS: 1.8 };
    expect(whatsLocked({ ...base, event, eventAgeMs: 100 }).word).toBe("Re-acquired · 0.86");
    expect(whatsLocked({ ...base, event, eventAgeMs: REACQUIRED_WORD_MS + 1 }).word).toBe("Locked");
  });

  it("falls back to the tracker's lock words", () => {
    expect(whatsLocked({ ...base, detection: null })).toEqual({
      who: "TARGET · #12",
      word: "Not in view",
      tone: "muted",
      confidence: "—",
    });
    expect(whatsLocked({ ...base, detection: { ...base.detection!, lockState: "uncertain" } }).word).toBe("Uncertain");
    expect(whatsLocked({ ...base, detection: { ...base.detection!, lockState: "lost" } }).tone).toBe("crit");
  });

  it("drops the id for a box designated without a track", () => {
    expect(whatsLocked({ ...base, designated: { ...base.designated, trackId: null } }).who).toBe("PERSON");
  });
});
