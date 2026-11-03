// The extension install review as a reducer: summary → permissions →
// installing. The approve slide stays locked until the gate opens: a high or
// critical grant needs the "understood" switch, and a critical grant (or a
// critical plugin) also waits out a cool-off. Pure, so the gate is testable.

import { requiresCoolOff, type PluginManifestSummary } from "@/shared/api/plugin-install";

export const COOL_OFF_SECONDS = 3;

export type ReviewStage = "summary" | "permissions" | "installing";

export interface ReviewState {
  stage: ReviewStage;
  understood: boolean;
  coolOff: number;
  /** Whether the manifest is high risk or worse (needs "understood"). */
  highRisk: boolean;
  /** Whether the manifest has a critical grant (needs the cool-off). */
  critical: boolean;
}

export type ReviewAction =
  | { type: "continue" }
  | { type: "understood"; value: boolean }
  | { type: "tick" }
  | { type: "installing" }
  | { type: "failed" };

export function initialReview(manifest: PluginManifestSummary): ReviewState {
  const critical = requiresCoolOff(manifest);
  return {
    stage: "summary",
    understood: false,
    coolOff: critical ? COOL_OFF_SECONDS : 0,
    highRisk: critical || manifest.risk === "high" || manifest.risk === "critical",
    critical,
  };
}

export function reviewReducer(state: ReviewState, action: ReviewAction): ReviewState {
  switch (action.type) {
    case "continue":
      // The cool-off starts when the permissions are first on screen.
      return state.stage === "summary"
        ? { ...state, stage: "permissions", coolOff: state.critical ? COOL_OFF_SECONDS : 0 }
        : state;
    case "understood":
      return { ...state, understood: action.value };
    case "tick":
      return state.stage === "permissions" && state.coolOff > 0 ? { ...state, coolOff: state.coolOff - 1 } : state;
    case "installing":
      return canApprove(state) ? { ...state, stage: "installing" } : state;
    case "failed":
      return state.stage === "installing" ? { ...state, stage: "permissions" } : state;
  }
}

/** Whether the approve slide is live. */
export function canApprove(state: ReviewState): boolean {
  return state.stage === "permissions" && state.coolOff === 0 && (!state.highRisk || state.understood);
}

/** The permissions the install requests: every required one. */
export function requiredPermissions(manifest: PluginManifestSummary): string[] {
  return manifest.permissions.filter((p) => p.required).map((p) => p.id);
}
