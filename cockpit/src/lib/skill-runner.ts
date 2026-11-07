// Executing a skill and requesting one through its confirm tier. Every input
// path (the left column, the skill picker, the follow rail, keyboard chords,
// controller bindings through the cockpit actions) goes through
// `requestSkill`, so the gates and the confirm tier are the same whichever
// source fired it. The outcome is reported truthfully: accepted, rejected with
// the reason, sent but not acknowledged, queued, or failed.
//
// Built-ins go to the drone's pilot (`action`) while a pilot state is present
// and its link is open, else to `POST /api/command`. Pause, land, rth and kill
// also stop every running motion skill (navigation and camera skills keep
// running). Kill never waits behind another command.
//
// Plugin skills write their activation key. A motion skill's GO first stops
// the running motion skill (preemption); a toggle write is pending until
// the plugin echoes it; a stop that fails because the link is down is queued
// and sent when the link returns.

import { skillContextNow } from "@/hooks/use-skill-context";
import { sendCommand } from "@/lib/api";
import type { ActionPhase } from "@/lib/cockpit-actions";
import { sendPilotAction } from "@/lib/pilot-link";
import { activateExclusive, runningMotionSkills } from "@/lib/skill-gates";
import { builtinRoute, resolveSkillState, type BuiltinBinding, type Skill, type SkillContext } from "@/lib/skills";
import { ApiError, errorDetail } from "@/shared/api-fetch";
import { useConfirmStore, type AckLine } from "@/stores/confirm-store";
import { activateExtensionSkill, pluginSkills, useExtensionsStore } from "@/stores/extensions-store";
import { requirePreflight } from "@/stores/preflight-store";

const ACK_CLEAR_MS = 4000;
let ackTimer: ReturnType<typeof setTimeout> | undefined;

function report(ack: AckLine): void {
  useConfirmStore.getState().setAck(ack);
  clearTimeout(ackTimer);
  ackTimer = setTimeout(() => useConfirmStore.getState().setAck(null), ACK_CLEAR_MS);
}

function failureText(e: unknown): string {
  return e instanceof ApiError ? errorDetail(e.body, e.message) : "command failed";
}

/** The request never reached the agent, or the agent's upstream is down: a
 *  stop written now is queued until the link returns. */
function isLinkFailure(e: unknown): boolean {
  return !(e instanceof ApiError) || e.status === 0 || e.status >= 502;
}

async function sendBuiltin(skill: Skill, b: BuiltinBinding, altitudeM: number | undefined): Promise<void> {
  const route = builtinRoute(skill, skillContextNow());
  if (route === "pilot") {
    const r = await sendPilotAction(b.pilotAction);
    report(r.ok ? { text: `${skill.label}: accepted`, kind: "ok" } : { text: `${skill.label}: ${r.err ?? "rejected"}`, kind: "err" });
    return;
  }
  if (route !== "command" || !b.command) {
    report({ text: `${skill.label}: no link`, kind: "err" });
    return;
  }
  const args = skill.takesAltitude && altitudeM != null ? [altitudeM] : [];
  const res = await sendCommand(b.command, args);
  const a = res.ack;
  if (!a?.observed) {
    report({ text: `${skill.label}: sent, no acknowledgement`, kind: "warn" });
  } else if (a.accepted) {
    report({ text: `${skill.label}: accepted`, kind: "ok" });
  } else {
    const why = a.statustext ? ` — ${a.statustext}` : "";
    report({ text: `${skill.label}: ${a.result_name ?? "rejected"}${why}`, kind: "err" });
  }
}

/** Write a plugin skill's activation key; a toggle's write is pending until
 *  echoed. A stop the link could not carry is queued, and resolves. */
async function writeActivation(skill: Skill, value: boolean): Promise<void> {
  const e = skill.extension!;
  const store = useExtensionsStore.getState();
  if (e.toggle) store.markPending(skill.id, value);
  try {
    await activateExtensionSkill(e.pluginId, e.configKey, value);
    store.dequeueStop(skill.id);
  } catch (err) {
    if (e.toggle && !value && isLinkFailure(err)) {
      store.queueStop(skill.id);
      report({ text: `${skill.label}: stop queued until the link returns`, kind: "warn" });
      return;
    }
    if (e.toggle) store.clearPending(skill.id);
    throw err;
  }
}

/** Resolve once the skill reports not active, or after `timeoutMs`. */
function waitIdle(skill: Skill, timeoutMs: number): Promise<void> {
  return new Promise((resolve) => {
    const idle = () => useExtensionsStore.getState().reported[skill.id]?.state !== "active";
    if (idle()) {
      resolve();
      return;
    }
    const finish = () => {
      clearTimeout(timer);
      unsubscribe();
      resolve();
    };
    const timer = setTimeout(finish, timeoutMs);
    const unsubscribe = useExtensionsStore.subscribe(() => {
      if (idle()) finish();
    });
  });
}

/** Stop one plugin skill (a tap: stopping makes the vehicle hold). */
export async function stopSkill(skill: Skill): Promise<void> {
  try {
    await writeActivation(skill, false);
  } catch (e) {
    report({ text: `${skill.label}: ${failureText(e)}`, kind: "err" });
  }
}

/** Stop every running motion skill: built-in pause, land, rth and kill, and
 *  any Mode-sheet change. Navigation and camera skills are untouched. */
export async function stopMotionSkills(): Promise<void> {
  const { reported, pending } = useExtensionsStore.getState();
  await Promise.all(runningMotionSkills(pluginSkills(), reported, pending).map(stopSkill));
}

let flushing = false;

/** Send the stops queued while the link was down. Called when a state poll
 *  succeeds again. */
export async function flushQueuedStops(): Promise<void> {
  if (flushing) return;
  flushing = true;
  try {
    for (const id of useExtensionsStore.getState().queuedStops) {
      const skill = pluginSkills().find((s) => s.id === id);
      if (!skill) {
        useExtensionsStore.getState().dequeueStop(id);
        continue;
      }
      await stopSkill(skill);
    }
  } finally {
    flushing = false;
  }
}

async function runExtension(skill: Skill, active: boolean): Promise<void> {
  const e = skill.extension!;
  if (e.toggle && active) {
    await stopSkill(skill);
    return;
  }
  if (e.toggle && e.motion) {
    const { reported, pending } = useExtensionsStore.getState();
    const running = runningMotionSkills(pluginSkills(), reported, pending);
    await activateExclusive(skill, running, { write: writeActivation, waitIdle });
  } else {
    await writeActivation(skill, true);
  }
  report({ text: `${skill.label}: sent`, kind: "ok" });
}

/** Pause and kill (the built-ins sent blind on a stale link) never wait behind
 *  another command's acknowledgement and never hold the others off: "Land,
 *  then Pause to abort" must brake at once. */
export function bypassesBusy(skill: Skill): boolean {
  return skill.builtin?.staleLink === "send_blind";
}

async function run(skill: Skill, opts: { altitudeM?: number; active?: boolean }): Promise<void> {
  const exempt = bypassesBusy(skill);
  const store = useConfirmStore.getState();
  if (store.busy && !exempt) return;
  if (!exempt) store.setBusy(true);
  try {
    if (skill.builtin) {
      const sent = sendBuiltin(skill, skill.builtin, opts.altitudeM);
      if (skill.builtin.stopsMotion) void stopMotionSkills();
      await sent;
    } else if (skill.extension) {
      await runExtension(skill, opts.active === true);
    }
  } catch (e) {
    report({ text: `${skill.label}: ${failureText(e)}`, kind: "err" });
  } finally {
    if (!exempt) useConfirmStore.getState().setBusy(false);
  }
}

/** Run a skill now (its confirm, if any, is already satisfied). Arm and
 *  takeoff pass the session's pre-flight checklist first. */
export async function executeSkill(skill: Skill, opts: { altitudeM?: number; active?: boolean } = {}): Promise<void> {
  if (skill.id === "arm" || skill.builtin?.armsFirst) {
    requirePreflight(() => void run(skill, opts));
    return;
  }
  await run(skill, opts);
}

/** Request a skill: gate it, then fire (tap, or stopping a running toggle) or
 *  open its confirm sheet. Returns false when the skill is not drivable now. */
export function requestSkill(skill: Skill, ctx: SkillContext, gamepadButton: number | null = null): boolean {
  const state = resolveSkillState(skill, ctx);
  const store = useConfirmStore.getState();
  if (!state.enabled || (store.busy && !bypassesBusy(skill))) return false;
  if (skill.confirm.kind === "tap" || (skill.extension?.toggle && state.active)) {
    void executeSkill(skill, { active: state.active });
    return true;
  }
  store.open(skill, gamepadButton);
  return true;
}

/**
 * A cockpit action bound to a skill (a controller binding or a chord):
 *   press   — gate, then fire a tap skill or open the confirm sheet; on an
 *             open sheet for the same skill, start its hold (or confirm);
 *   hold    — a hold the controller timed completed: complete the sheet's
 *             hold (opening it first), which for kill moves to its confirm;
 *   release — end a hold on the open sheet.
 * `heldPress` starts the sheet's hold on the press itself (a held key).
 */
export function runSkillAction(skill: Skill, phase: ActionPhase, ctx: SkillContext, heldPress = false): void {
  const confirm = useConfirmStore.getState();
  const open = confirm.pending?.skill.id === skill.id ? confirm.pending : null;
  if (phase === "release") {
    if (open) confirm.setHeld(false);
    return;
  }
  if (phase === "press") {
    if (open) {
      if (open.stage === "confirm") confirm.panelConfirm();
      else confirm.setHeld(true);
      return;
    }
    if (requestSkill(skill, ctx) && heldPress && useConfirmStore.getState().pending?.skill.id === skill.id) {
      useConfirmStore.getState().setHeld(true);
    }
    return;
  }
  if (open?.stage === "confirm") return;
  if (!open) {
    if (skill.confirm.kind === "tap") {
      requestSkill(skill, ctx);
      return;
    }
    if (!requestSkill(skill, ctx)) return;
  }
  useConfirmStore.getState().panelConfirm();
}
