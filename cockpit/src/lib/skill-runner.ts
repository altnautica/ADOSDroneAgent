// Executing a skill and requesting one through its confirm tier. Every input
// path (the left column, the skill picker, the follow rail, keyboard chords,
// controller bindings through the cockpit actions) goes through
// `requestSkill`, so the gates and the confirm tier are the same whichever
// source fired it. The outcome is reported truthfully: accepted, rejected with
// the reason, sent but not acknowledged, queued, or failed. A press that is
// dropped (the skill is not drivable now, or its previous press still awaits
// an answer) says why in a toast; it is never silent.
//
// Built-ins go to the drone's pilot (`action`) while a pilot state is present
// and its link is open, else to `POST /api/command`. Both paths answer within
// three seconds, a missing answer included. Pause, land, rth and kill also
// stop every running motion skill (navigation and camera skills keep
// running).
//
// A press waits only behind its own skill's unanswered command on this drone
// (busy per drone and skill); Return home, Land, Pause and Emergency stop
// never wait at all.
//
// Plugin skills write their activation key. A motion skill's GO first stops
// the running motion skill (preemption); a toggle write is pending until
// the plugin echoes it; a stop that fails because the link is down is queued
// and sent when the link returns.

import { create } from "zustand";

import { skillContextNow } from "@/hooks/use-skill-context";
import { CommandTimeoutError, sendCommand } from "@/lib/api";
import type { ActionPhase } from "@/lib/cockpit-actions";
import { sendPilotAction } from "@/lib/pilot-link";
import { refusalText } from "@/shared/hud/refusal";
import { hasSpecText, specText } from "@/shared/hud/spec-text";
import { activateExclusive, runningMotionSkills } from "@/lib/skill-gates";
import { builtinRoute, resolveSkillState, type BuiltinBinding, type Skill, type SkillContext } from "@/lib/skills";
import { ApiError, errorDetail } from "@/shared/api-fetch";
import { toast } from "@/shared/api/toast";
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
  if (e instanceof CommandTimeoutError) return refusalText(e.code);
  if (!(e instanceof ApiError)) return refusalText("no response");
  const code = e.code;
  return code && hasSpecText(`refusal.${code}`) ? refusalText(code) : errorDetail(e.body, e.message);
}

/** A `COMMAND_ACK` result name in the spec's words (`mav_result.*`). */
function mavResultText(name: string | undefined): string {
  return name && hasSpecText(`mav_result.${name}`) ? specText(`mav_result.${name}`) : refusalText(null);
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
    report(r.ok ? { text: `${skill.label}: ${mavResultText("ACCEPTED")}`, kind: "ok" } : { text: `${skill.label}: ${refusalText(r.err)}`, kind: "err" });
    return;
  }
  if (route !== "command" || !b.command) {
    report({ text: `${skill.label}: ${refusalText("no_fc")}`, kind: "err" });
    return;
  }
  const args = skill.takesAltitude && altitudeM != null ? [altitudeM] : [];
  let res;
  try {
    res = await sendCommand(b.command, args);
  } catch (e) {
    // The command route refuses with a status, not a code: 503 is no flight
    // controller link, 400 a value it does not take.
    if (e instanceof ApiError && (e.status === 503 || e.status === 400)) {
      report({ text: `${skill.label}: ${refusalText(e.status === 503 ? "no_fc" : "bad_args")}`, kind: "err" });
      return;
    }
    throw e;
  }
  const a = res.ack;
  if (!a?.observed) {
    report({ text: specText("skill.did_not_respond", { skill: skill.label }), kind: "warn" });
  } else if (a.accepted) {
    report({ text: `${skill.label}: ${mavResultText("ACCEPTED")}`, kind: "ok" });
  } else {
    const why = a.statustext ? ` — ${a.statustext}` : "";
    report({ text: `${skill.label}: ${mavResultText(a.result_name)}${why}`, kind: "err" });
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
      report({ text: specText("skill.stop_queued_link", { skill: skill.label }), kind: "warn" });
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
  report({ text: `${skill.label}: ${mavResultText("ACCEPTED")}`, kind: "ok" });
}

/** Return home, Land, Pause and Emergency stop (the built-ins that stop motion)
 *  never wait behind a command and never hold one off: "Take off, then Return
 *  home" and "Land, then Pause to abort" act at once. */
export function bypassesBusy(skill: Skill): boolean {
  return skill.builtin?.stopsMotion === true;
}

/** The drone this cockpit commands: its own node (a ground station commands
 *  the one aircraft it is paired with). Part of the busy key, as on every
 *  surface that keys busy by drone and skill. */
const DRONE = "self";

/** The (drone, skill) pairs whose command awaits its answer. */
export const useSkillBusyStore = create<{ keys: Readonly<Record<string, true>> }>(() => ({ keys: {} }));

function busyKey(skill: Skill): string {
  return `${DRONE}::${skill.id}`;
}

function setSkillBusy(skill: Skill, on: boolean): void {
  useSkillBusyStore.setState((s) => {
    const keys = { ...s.keys };
    if (on) keys[busyKey(skill)] = true;
    else delete keys[busyKey(skill)];
    return { keys };
  });
}

/** Whether a press of `skill` now would be dropped because its own previous
 *  command still awaits an answer. */
export function isSkillBusy(skill: Skill): boolean {
  return !bypassesBusy(skill) && useSkillBusyStore.getState().keys[busyKey(skill)] === true;
}

/** React: the same, for a control that dims while its command is out. */
export function useSkillBusy(skill: Skill | null | undefined): boolean {
  return useSkillBusyStore((s) => skill != null && !bypassesBusy(skill) && s.keys[busyKey(skill)] === true);
}

/** A press that does not go out says why. */
function dropped(skill: Skill, why: string): void {
  toast.info(`${skill.label}: ${why}`);
}

async function run(skill: Skill, opts: { altitudeM?: number; active?: boolean }): Promise<void> {
  if (isSkillBusy(skill)) {
    dropped(skill, refusalText("busy"));
    return;
  }
  // A built-in's gate holds on every route at the moment it is sent, not only
  // when its sheet opened: a Disarm confirmed after take-off is not sent. A
  // plugin skill's stop is never held back.
  if (skill.builtin) {
    const state = resolveSkillState(skill, skillContextNow());
    if (!state.enabled) {
      dropped(skill, state.reason ?? refusalText(null));
      return;
    }
  }
  const exempt = bypassesBusy(skill);
  if (!exempt) setSkillBusy(skill, true);
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
    if (!exempt) setSkillBusy(skill, false);
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
 *  open its confirm sheet. Returns false, with the reason in a toast, when the
 *  skill is not drivable now or its previous press still awaits an answer. */
export function requestSkill(skill: Skill, ctx: SkillContext, gamepadButton: number | null = null): boolean {
  const state = resolveSkillState(skill, ctx);
  if (!state.enabled) {
    dropped(skill, state.reason ?? refusalText(null));
    return false;
  }
  if (isSkillBusy(skill)) {
    dropped(skill, refusalText("busy"));
    return false;
  }
  if (skill.confirm.kind === "tap" || (skill.extension?.toggle && state.active)) {
    void executeSkill(skill, { active: state.active });
    return true;
  }
  useConfirmStore.getState().open(skill, gamepadButton);
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
