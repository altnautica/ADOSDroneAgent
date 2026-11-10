// What a pending pick in the Mode sheet holds back, and how a change goes
// out. A pick waits only in its own row; the flight controller's recovery
// holds and handing the aircraft to the flight controller never wait behind
// anything. Running motion skills are stopped alongside the change, never
// ahead of it, so a plugin that does not answer cannot hold the change back.

import { sendCommand } from "@/lib/api";
import { mavResultText } from "@/lib/cockpit-flow";
import { FC_FAILSAFE_MODES, FC_RECOVERY_MODES, noteCommandedModes } from "@/lib/fc-failsafe";
import { stopMotionSkills } from "@/lib/skill-runner";
import { specText } from "@/shared/hud/spec-text";

export interface Outcome {
  ok: boolean;
  reason?: string;
}

/** The value waiting for an answer, by row. */
export type PendingRows = Readonly<Record<string, string>>;

/** Whether a control is held back by a pick pending in its own row. The
 *  recovery holds and handing to the flight controller never are. */
export function pickDisabled(pending: PendingRows, row: string, value: string): boolean {
  if ((row === "fc" && FC_RECOVERY_MODES[value] === true) || (row === "who" && value !== "take")) return false;
  return pending[row] !== undefined;
}

/** Send a change, stopping the running motion skills alongside it. */
export function modeChange(run: () => Promise<Outcome>, stop: () => Promise<void> = stopMotionSkills): Promise<Outcome> {
  void stop().catch(() => undefined);
  return run();
}

/** Set a flight-controller mode (`POST /api/command` `mode`). A return or
 *  landing sent from here is recorded as this cockpit's, not a failsafe. */
export async function sendFcMode(name: string): Promise<Outcome> {
  if (FC_FAILSAFE_MODES[name]) noteCommandedModes([name]);
  try {
    const r = await sendCommand("mode", [name]);
    if (!r.ack?.observed || r.ack.accepted) return { ok: true };
    return { ok: false, reason: r.ack.result_name ? mavResultText(r.ack.result_name) : specText("flow.mode.fc.rejected") };
  } catch {
    return { ok: false, reason: specText("flow.mode.fc.failed") };
  }
}
