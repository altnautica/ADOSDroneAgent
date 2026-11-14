// The waiting-for-instructions prompt's actions (follow-me holding for the
// operator) and their transports: search again writes the next
// `operator_resume_seq` to the follow-me config, pick a target arms the
// click-a-box designate flow, return home and land go to the pilot, and stop
// writes `active = false`. The transports are injected, so the dispatcher is
// pure and every route is unit-tested.
//
// Keys: R search again, P pick, H home, L land, X stop. They answer only while
// the prompt has focus and only without a modifier: the built-ins own the
// Shift chords (Shift+R return home, Shift+L land, Shift+X kill) and a plugin
// may bind a bare letter globally (a camera pod's `R` record, `L` laser), so
// the prompt never takes a key from the window.

import type { ActionResult } from "@/lib/pilot-link";
import type { FollowPromptAction } from "@/shared/hud/follow-prompt";

export const FOLLOW_ME_PLUGIN_ID = "com.altnautica.follow-me";

export interface FollowActionDeps {
  /** Write one plugin config key (`PUT /api/plugins/{id}/config`). */
  put: (pluginId: string, key: string, value: unknown) => Promise<unknown>;
  /** Send a pilot action; resolves with its result, never rejects. */
  pilot: (name: string) => Promise<ActionResult>;
  /** Arm the pick-a-target mode. */
  startPick: () => void;
  /** The `operator_resume_seq` follow-me last reported. */
  seq: number;
}

async function write(d: FollowActionDeps, key: string, value: unknown): Promise<ActionResult> {
  try {
    await d.put(FOLLOW_ME_PLUGIN_ID, key, value);
    return { ok: true };
  } catch (e) {
    return { ok: false, err: e instanceof Error ? e.message : String(e) };
  }
}

/** Run one prompt action through its transport. Never rejects. */
export async function runFollowAction(action: FollowPromptAction, d: FollowActionDeps): Promise<ActionResult> {
  switch (action) {
    case "resume":
      return write(d, "operator_resume_seq", d.seq + 1);
    case "redesignate":
      d.startPick();
      return { ok: true };
    case "rth":
    case "land":
      return d.pilot(action);
    case "stop":
      return write(d, "active", false);
  }
}

export const FOLLOW_ACTION_KEYS: Readonly<Record<FollowPromptAction, string>> = {
  resume: "r",
  redesignate: "p",
  rth: "h",
  land: "l",
  stop: "x",
};

/** The offered action a bare key names, or null (any modifier, or a key of an
 *  action the prompt does not offer). */
export function followActionForKey(
  e: { key: string; shiftKey: boolean; ctrlKey: boolean; altKey: boolean; metaKey: boolean },
  offered: readonly FollowPromptAction[],
): FollowPromptAction | null {
  if (e.shiftKey || e.ctrlKey || e.altKey || e.metaKey) return null;
  const key = e.key.toLowerCase();
  return offered.find((a) => FOLLOW_ACTION_KEYS[a] === key) ?? null;
}
