// The radio bind session as the Pair screen shows it: the supervisor's bind
// state mapped to a label key, how long it has sat in that step, the reason a
// bind failed, and how a finished bind ends (for the result toast).

/** The bind session as `GET`/`POST /api/wfb/pair/local-bind` report it. */
export interface BindSession {
  state?: string;
  phase?: string;
  phase_age_s?: number | null;
  error?: string | null;
  [k: string]: unknown;
}

const PHASE_KEYS = {
  idle: "cockpit.pair.bindPhase.idle",
  opening_tunnel: "cockpit.pair.bindPhase.opening_tunnel",
  waiting_peer: "cockpit.pair.bindPhase.waiting_peer",
  transferring_keys: "cockpit.pair.bindPhase.transferring_keys",
  applying_keys: "cockpit.pair.bindPhase.applying_keys",
  restarting_services: "cockpit.pair.bindPhase.restarting_services",
  paired: "cockpit.pair.bindPhase.paired",
  failed: "cockpit.pair.bindPhase.failed",
  aborted: "cockpit.pair.bindPhase.aborted",
} as const;

export type BindPhaseKey = (typeof PHASE_KEYS)[keyof typeof PHASE_KEYS] | "cockpit.pair.bindPhase.unknown";

export interface BindView {
  phaseKey: BindPhaseKey;
  /** Whole seconds in the current step, or null when not reported. */
  ageS: number | null;
  /** The failure reason, when the session carries one. */
  error: string | null;
}

/** The session's view, or null when there is no session or it is idle. */
export function bindView(b: BindSession | null): BindView | null {
  if (!b) return null;
  const state = b.state ?? b.phase;
  if (typeof state !== "string" || state === "" || state === "idle") return null;
  const phaseKey = state in PHASE_KEYS ? PHASE_KEYS[state as keyof typeof PHASE_KEYS] : "cockpit.pair.bindPhase.unknown";
  const age = typeof b.phase_age_s === "number" && Number.isFinite(b.phase_age_s) ? Math.round(b.phase_age_s) : null;
  const error = typeof b.error === "string" && b.error ? b.error : null;
  return { phaseKey, ageS: age, error };
}

/** How a finished bind ended, from the POST reply's session: `paired`,
 *  `failed` (with the reason), or `aborted`; null while it is still running. */
export function bindOutcome(b: BindSession | null): { kind: "paired" | "failed" | "aborted"; reason: string | null } | null {
  const state = b?.state ?? b?.phase;
  if (state === "paired" || state === "done") return { kind: "paired", reason: null };
  if (state === "failed") return { kind: "failed", reason: typeof b?.error === "string" ? b.error : null };
  if (state === "aborted" || state === "cancelled") return { kind: "aborted", reason: null };
  return null;
}
