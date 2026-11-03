// The what's-locked chip (13-hud-v2 §8.4): who is designated, the one state
// word that matters now, and the live confidence. The first matching rule
// wins: a stale feed, then a fresh follow phase, then a re-acquire within the
// last 4 s, then the tracker's own lock words. Pure.

import { classWord, FOLLOW_FRESH_MS, holdReasonWord, type DesignatedRef, type TargetDetection } from "@/shared/hud/target-state";
import type { FollowEvent, FollowState } from "@/shared/hud/nav-types";

/** How long the chip keeps the re-acquired word. */
export const REACQUIRED_WORD_MS = 4000;

export type ChipTone = "track" | "coast" | "warn" | "crit" | "muted";

export interface WhatsLockedInput {
  designated: DesignatedRef;
  /** The designated target's detection in the fresh batch for its camera,
   *  or null when it is not in view. */
  detection: TargetDetection | null;
  /** Why the feed cannot show the target now (`No video`, `Waiting for
   *  feed`), or null when it can. */
  staleReason: string | null;
  follow: FollowState | null;
  followAgeMs: number;
  event: FollowEvent | null;
  eventAgeMs: number;
}

export interface WhatsLocked {
  who: string;
  word: string;
  tone: ChipTone;
  confidence: string;
}

export function whatsLocked(i: WhatsLockedInput): WhatsLocked {
  const cls = i.detection ? classWord(i.detection.classLabel).trim() || "TARGET" : "TARGET";
  const id = i.designated.trackId;
  const who = id != null ? `${cls} · #${id}` : cls;
  const confidence = i.detection ? `${Math.round(i.detection.confidence * 100)}%` : "—";
  const out = (word: string, tone: ChipTone): WhatsLocked => ({ who, word, tone, confidence });

  if (i.staleReason) return out(i.staleReason, "muted");

  const f = i.follow && i.followAgeMs <= FOLLOW_FRESH_MS ? i.follow : null;
  if (f && f.phase !== "idle") {
    switch (f.phase) {
      case "following":
        return out("Following", "track");
      case "acquiring":
        return out("Acquiring", "track");
      case "coasting":
        return out(`Coasting ${(f.coastRemainingS ?? 0).toFixed(1)} s`, "coast");
      case "searching":
        return out("Searching", "coast");
      case "holding": {
        const reason = holdReasonWord(f.holdReason);
        return out(reason ? `Holding · ${reason.charAt(0)}${reason.slice(1).toLowerCase()}` : "Holding", "warn");
      }
      case "lost":
        return out("Lost", "crit");
    }
  }

  if (i.event?.topic === "follow.reacquired" && i.eventAgeMs <= REACQUIRED_WORD_MS) {
    return out(`Re-acquired · ${i.event.similarity.toFixed(2)}`, "track");
  }

  if (!i.detection) return out("Not in view", "muted");
  if (i.detection.lockState === "uncertain") return out("Uncertain", "warn");
  if (i.detection.lockState === "lost") return out("Lost", "crit");
  return out("Locked", "track");
}
