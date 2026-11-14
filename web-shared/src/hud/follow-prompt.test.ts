import { describe, expect, it } from "vitest";

import strings from "../cockpit-spec/setup-strings.json";
import { PHASE_TONE } from "./follow-chrome";
import { followPrompt, FOLLOW_PROMPT_ACTIONS } from "./follow-prompt";
import cases from "./fixtures/follow-prompt-cases.json";
import { parseFollowEvent, parseFollowState, PHASES, RECOVERY_PHASES } from "./nav-types";

const S: Record<string, string> = strings.strings;

describe("followPrompt", () => {
  for (const c of cases) {
    it(c.name, () => {
      expect(followPrompt(parseFollowState(c.state)!)).toEqual(c.expect);
    });
  }

  it("has a spec string for every title, body and action it can return", () => {
    for (const c of cases) {
      if (!c.expect) continue;
      expect(S[c.expect.titleKey], c.expect.titleKey).toBeTruthy();
      expect(S[c.expect.bodyKey], c.expect.bodyKey).toBeTruthy();
    }
    for (const a of FOLLOW_PROMPT_ACTIONS) expect(S[`follow.prompt.action.${a}`], a).toBeTruthy();
    expect(S["follow.prompt.await_title"]).toBe("Target lost: holding, waiting for instructions");
  });
});

describe("0.5 follow phases", () => {
  it("parses the recovery fields", () => {
    const s = parseFollowState({
      active: true,
      phase: "searching",
      search_step: 1,
      reacquire_remaining_s: 12.5,
      search_budget_s: 25,
      search_area: { center_ne: [1, 2], semi_major_m: 3, semi_minor_m: 1, heading_deg: 40 },
      vantage_ne: [5, 6],
      vantage_state: "arrived",
      occluder: { ne: [2, 2], kind: "narrow" },
      predicted_path_ne: [[1, 2], [1.5, 2.2]],
      hold_ne: null,
      uncertain_mode: "hold",
      predictive_search: false,
      operator_resume_seq: 3,
      outpaced: true,
      speed_limit_mps: 6,
    })!;
    expect(s.searchArea).toEqual({ centerNe: [1, 2], semiMajorM: 3, semiMinorM: 1, headingDeg: 40 });
    expect(s.occluder).toEqual({ ne: [2, 2], kind: "narrow" });
    expect(s.reacquireRemainingS).toBe(12.5);
    expect(s.searchBudgetS).toBe(25);
    expect(s.searchStep).toBe(1);
    expect(s.vantageNe).toEqual([5, 6]);
    expect(s.vantageState).toBe("arrived");
    expect(s.predictedPathNe).toEqual([[1, 2], [1.5, 2.2]]);
    expect(s.holdNe).toBeNull();
    expect(s.uncertainMode).toBe("hold");
    expect(s.predictiveSearch).toBe(false);
    expect(s.operatorResumeSeq).toBe(3);
    expect(s.outpaced).toBe(true);
    expect(s.speedLimitMps).toBe(6);
  });

  it("drops malformed recovery fields instead of guessing", () => {
    const s = parseFollowState({
      phase: "holding-await-operator",
      search_area: { center_ne: [1], semi_major_m: 3, semi_minor_m: 1, heading_deg: 40 },
      occluder: { ne: [2, 2], kind: "tall" },
      vantage_state: "parked",
      predicted_path_ne: [[1, 2], ["x", 3], [4, 5]],
      hold_ne: [7.9, 2.4],
    })!;
    expect(s.phase).toBe("holding-await-operator");
    expect(s.searchArea).toBeNull();
    expect(s.occluder).toBeNull();
    expect(s.vantageState).toBeNull();
    expect(s.predictedPathNe).toEqual([[1, 2], [4, 5]]);
    expect(s.holdNe).toEqual([7.9, 2.4]);
    expect(s.searchStep).toBeNull();
    expect(s.operatorResumeSeq).toBe(0);
  });

  it("parses the 0.5 events", () => {
    expect(
      parseFollowEvent("follow.reacquired", { track_id: 9, similarity: 0.81, gap_s: 4.2, mahalanobis: 1.3, phase: "searching" }),
    ).toEqual({ topic: "follow.reacquired", trackId: 9, similarity: 0.81, gapS: 4.2, mahalanobis: 1.3, phase: "searching" });
    expect(
      parseFollowEvent("follow.lost", { track_id: 9, reason: "search_exhausted", awaiting_operator: true, hold_ne: [7.9, 2.4] }),
    ).toEqual({ topic: "follow.lost", trackId: 9, reason: "search_exhausted", awaitingOperator: true, holdNe: [7.9, 2.4] });
    expect(parseFollowEvent("follow.stopped", { reason: "lease-revoked" })).toEqual({
      topic: "follow.stopped",
      reason: "lease-revoked",
    });
  });

  it("every phase has a tone and a word", () => {
    for (const p of PHASES) {
      expect(PHASE_TONE[p], p).toBeDefined();
      expect(S[`follow.phase.${p}`], p).toBeTruthy();
    }
    expect(PHASE_TONE["holding-await-operator"]).toBe("crit");
    for (const p of RECOVERY_PHASES) expect(PHASE_TONE[p]).toBe("coast");
  });

  it("coasting and lost are gone", () => {
    expect(PHASES).not.toContain("coasting" as never);
    expect(PHASES).not.toContain("lost" as never);
    expect(parseFollowState({ active: true, phase: "coasting" })?.phase).toBe("acquiring");
    expect(S["follow.phase.coasting"]).toBeUndefined();
    expect(S["follow.phase.lost"]).toBeUndefined();
  });

  it("names every hold reason, vantage state and occluder the follow emits", () => {
    for (const r of [
      "inactive",
      "no-lock",
      "lock-uncertain",
      "awaiting-operator",
      "search-exhausted",
      "predictive-search-off",
      "pose-stale",
      "fc-stale",
      "fc-disarmed",
      "no-range",
      "lease-busy",
      "lease-refused",
    ]) {
      expect(S[`follow.hold_reason.${r}`], r).toBeTruthy();
    }
    for (const v of ["enroute", "arrived", "no-vantage"]) expect(S[`follow.vantage.${v}`], v).toBeTruthy();
    for (const k of ["narrow", "wide"]) expect(S[`follow.occluder.${k}`], k).toBeTruthy();
    for (const k of ["uncertain_mode", "predictive_search"]) expect(S[`follow.setting.${k}`], k).toBeTruthy();
  });
});
