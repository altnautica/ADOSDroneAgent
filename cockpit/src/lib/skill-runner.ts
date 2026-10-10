// Executing a skill and requesting one through its confirm gesture. Every
// input path (Skill Bar tap, gamepad binding, keyboard chord) goes through
// `requestSkill`, so the gates and the confirm tier are the same whichever
// source fired it. The outcome is reported truthfully: accepted, rejected with
// the FC's reason, sent but not acknowledged, or failed.

import { sendCommand } from "@/lib/api";
import { resolveSkillState, type Skill, type SkillContext } from "@/lib/skills";
import { ApiError, errorDetail } from "@/shared/api-fetch";
import { useConfirmStore, type AckLine } from "@/stores/confirm-store";
import { activateExtensionSkill } from "@/stores/extensions-store";

const ACK_CLEAR_MS = 4000;
let ackTimer: ReturnType<typeof setTimeout> | undefined;

function report(ack: AckLine): void {
  const store = useConfirmStore.getState();
  store.setAck(ack);
  clearTimeout(ackTimer);
  ackTimer = setTimeout(() => useConfirmStore.getState().setAck(null), ACK_CLEAR_MS);
}

function failureText(e: unknown): string {
  return e instanceof ApiError ? errorDetail(e.body, e.message) : "command failed";
}

/** Run a skill now (its confirm, if any, is already satisfied). */
export async function executeSkill(
  skill: Skill,
  opts: { altitudeM?: number; active?: boolean } = {},
): Promise<void> {
  const store = useConfirmStore.getState();
  if (store.busy) return;
  store.setBusy(true);
  try {
    if (skill.extension) {
      const { pluginId, configKey, toggle } = skill.extension;
      await activateExtensionSkill(pluginId, configKey, toggle ? !opts.active : true);
      report({ text: `${skill.label}: sent`, kind: "ok" });
      return;
    }
    if (!skill.command) return;
    const args =
      skill.takesAltitude && opts.altitudeM != null ? [opts.altitudeM] : skill.command.args;
    const res = await sendCommand(skill.command.cmd, args);
    const a = res.ack;
    if (!a?.observed) {
      report({ text: `${skill.label}: sent, no acknowledgement`, kind: "warn" });
    } else if (a.accepted) {
      report({ text: `${skill.label}: accepted`, kind: "ok" });
    } else {
      const why = a.statustext ? ` — ${a.statustext}` : "";
      report({ text: `${skill.label}: ${a.result_name ?? "rejected"}${why}`, kind: "err" });
    }
  } catch (e) {
    report({ text: `${skill.label}: ${failureText(e)}`, kind: "err" });
  } finally {
    useConfirmStore.getState().setBusy(false);
  }
}

/** Request a skill: gate it, then fire (tap) or open its confirm sheet. A
 *  guarded skill arms its guard on this first activation. Returns false when
 *  the skill is not drivable right now. */
export function requestSkill(
  skill: Skill,
  ctx: SkillContext,
  gamepadButton: number | null = null,
): boolean {
  const state = resolveSkillState(skill, ctx);
  const store = useConfirmStore.getState();
  if (!state.enabled || store.busy) return false;
  if (skill.gesture === "tap") {
    void executeSkill(skill, { active: state.active });
    return true;
  }
  store.open(skill, gamepadButton);
  if (skill.gesture === "guarded") store.armGuard();
  return true;
}
