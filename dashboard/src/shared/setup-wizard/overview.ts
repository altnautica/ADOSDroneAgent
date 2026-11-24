// The Aircraft setup overview: the wizard's steps grouped into component
// cards (Frame, Sensors, Radio, ...), each card's state and one-line summary,
// the progress count, and which primary button the overview offers. Pure
// functions over the wire types, so every surface derives the same thing.

import type { TFunction } from "i18next";

import type { ReadinessItem } from "../hud/cockpit-frames";
import { formatValue, hasSetupText, optionText, setupText } from "./strings";
import type { SetupStatus, SetupStep, Wizard, WizardState } from "./types";

/** The vehicle class in words (`setupWizard.vehicleClass.*`), else as sent. */
export function vehicleClassLabel(t: TFunction, cls: string | null): string | null {
  return cls === "multirotor" || cls === "fixedwing" || cls === "vtol" ? t(`setupWizard.vehicleClass.${cls}`) : cls;
}

/** Flight-controller firmware names as their projects write them. */
const FIRMWARE_NAMES: Readonly<Record<string, string>> = {
  ardupilot: "ArduPilot",
  arducopter: "ArduCopter",
  arduplane: "ArduPlane",
  px4: "PX4",
  betaflight: "Betaflight",
  inav: "INAV",
};

/** A firmware name as its project writes it ("ardupilot" → "ArduPilot"),
 *  else as sent. */
export function firmwareLabel(fw: string | null): string | null {
  if (!fw) return null;
  return FIRMWARE_NAMES[fw.toLowerCase()] ?? fw;
}

export type CardState = "done" | "attention" | "in_progress" | "not_set" | "optional" | "not_needed" | "after_setup";

export interface CardDef {
  id: string;
  /** The step ids the card covers, in wizard order. */
  steps: readonly string[];
  /** A card the operator may leave unset (shows "Optional", not "Not set"). */
  optional?: boolean;
}

/** The `ground` wizard's cards. A card whose steps the adapter does not list
 *  is hidden; a step no card names (the automatic `detect`) is not shown. */
export const GROUND_CARDS: readonly CardDef[] = [
  { id: "frame", steps: ["frame"] },
  { id: "sensors", steps: ["accel", "level", "gyro", "compass", "airspeed"] },
  { id: "radio", steps: ["radio", "rc_ados_switch"] },
  { id: "outputs", steps: ["outputs"] },
  { id: "battery", steps: ["battery"] },
  { id: "flight", steps: ["initial_params", "notch_static"] },
  { id: "safety", steps: ["ados_profile"] },
  { id: "ados", steps: ["ados_intro"], optional: true },
];

export interface SetupCard {
  id: string;
  /** Spec text key of the card title. */
  title_key: string;
  steps: SetupStep[];
  state: CardState;
  /** Why the flight controller says this card needs attention now (a failing
   *  readiness item's message), or null. */
  flag: string | null;
}

/** The blocker an operator abort leaves on the step it stopped. */
export const CANCELLED_BLOCKER = "setup.blocker.cancelled";

/** A step the operator aborted: failed with only the cancelled blocker. It
 *  reads "Cancelled", neutral, and needs no attention. */
export function isCancelled(s: SetupStep): boolean {
  return s.state === "failed" && s.blockers.length > 0 && s.blockers.every((b) => b === CANCELLED_BLOCKER);
}

/** A step the wizard has moved past: done, or skipped as not needed. */
function finished(s: SetupStep): boolean {
  return s.state === "done" || s.state === "skipped";
}

/** A card's state from its steps: a failed or blocked step needs attention
 *  (an operator cancel does not); an active step is in progress; all skipped
 *  is not needed; all finished is done; otherwise not set (or optional). */
export function cardState(steps: readonly SetupStep[], optional = false): CardState {
  if (steps.some((s) => !isCancelled(s) && (s.state === "failed" || (s.blockers.length > 0 && !finished(s))))) {
    return "attention";
  }
  if (steps.some((s) => s.state === "running" || s.state === "waiting_user")) return "in_progress";
  if (steps.length > 0 && steps.every((s) => s.state === "skipped")) return "not_needed";
  if (steps.length > 0 && steps.every(finished)) return "done";
  return optional ? "optional" : "not_set";
}

/** The steps as the overview counts them. A wizard filed as done (by a run,
 *  or recorded complete with no per-step records) counts every step it has
 *  no record of as done; a step with a state of its own keeps it. */
export function filedSteps(wizard: Wizard): SetupStep[] {
  if (wizard.state !== "done") return wizard.steps;
  return wizard.steps.map((s) => (s.state === "pending" ? { ...s, state: "done" } : s));
}

/** The setup steps the flight controller's readiness says need fixing now,
 *  each with what is wrong: its parameter rows as `NAME have → want`, else
 *  the item's message. An `apply_profile` fix flags the ADOS profile step, a
 *  `setup_step` fix its step. Passing items flag nothing. */
export function readinessFlags(readiness: readonly ReadinessItem[] | null | undefined): Map<string, string> {
  const flags = new Map<string, string>();
  for (const r of readiness ?? []) {
    if (r.ok || !r.fix) continue;
    const step = r.fix.kind === "apply_profile" ? "ados_profile" : r.fix.kind === "setup_step" ? r.fix.step : null;
    if (!step || flags.has(step)) continue;
    const rows = (r.params ?? []).map((p) => `${p.name} ${formatValue(p.have)} → ${p.want}`);
    flags.set(step, rows.length ? rows.join(" · ") : r.msg);
  }
  return flags;
}

function makeCard(
  wizard: Wizard,
  id: string,
  title_key: string,
  steps: SetupStep[],
  flags: ReadonlyMap<string, string>,
  optional = false,
): SetupCard {
  let state = cardState(steps, optional);
  const flag = steps.map((s) => flags.get(s.step)).find((f) => f !== undefined) ?? null;
  // The FC's own report outranks a stored "done": its values drifted since.
  if (flag !== null && state !== "in_progress") state = "attention";
  // A wizard that waits on the ground wizard: its unset cards read "After setup".
  const waitsOnGround = wizard.blockers.some(
    (b) => b === "setup.blocker.ground_not_done" || b === "setup.blocker.ground_required",
  );
  if (state === "not_set" && waitsOnGround) state = "after_setup";
  return { id, title_key, steps, state, flag };
}

/** The wizard's cards: the ground wizard by component, any other wizard one
 *  card per step. `flags` (from `readinessFlags`) marks the cards whose
 *  steps the flight controller reports as needing a fix. */
export function wizardCards(wizard: Wizard, flags: ReadonlyMap<string, string> = new Map()): SetupCard[] {
  const all = filedSteps(wizard);
  if (wizard.id !== "ground") {
    return all.map((s) => makeCard(wizard, s.step, s.title_key, [s], flags));
  }
  const byId = new Map(all.map((s) => [s.step, s]));
  const cards: SetupCard[] = [];
  for (const def of GROUND_CARDS) {
    const steps = def.steps.map((id) => byId.get(id)).filter((s): s is SetupStep => s !== undefined);
    if (steps.length === 0) continue;
    cards.push(makeCard(wizard, def.id, `setup.card.${def.id}.title`, steps, flags, def.optional));
  }
  return cards;
}

/** The steps the overview counts and the guided view walks, in order. */
export function guidedSteps(wizard: Wizard): SetupStep[] {
  return wizardCards(wizard).flatMap((c) => c.steps);
}

/** "N of M done": finished (done or not needed) steps out of the shown ones. */
export function wizardProgress(wizard: Wizard): { done: number; total: number } {
  const steps = guidedSteps(wizard);
  return { done: steps.filter(finished).length, total: steps.length };
}

/** The step a card opens: a flagged step first (what the FC says is wrong),
 *  else its first unfinished step, else its first. */
export function cardTarget(card: SetupCard, flags: ReadonlyMap<string, string> = new Map()): SetupStep {
  return card.steps.find((s) => flags.has(s.step)) ?? card.steps.find((s) => !finished(s)) ?? card.steps[0];
}

/** The step a run in progress is on in this wizard, from the status's
 *  top-level `running` (a whole-wizard run, or one step run on its own after
 *  the wizard finished), else null. */
export function runningStep(status: SetupStatus, wizard: Wizard): string | null {
  return status.running?.wizard === wizard.id ? status.running.step : null;
}

export type PrimaryKind = "start" | "resume" | "continue" | "run_again";

/** The overview's primary button: Continue (resume an interrupted run, or open
 *  the run in progress), Start, or Run again. `running` is true while a run
 *  of this wizard is in progress, whatever its filed state. */
export function overviewPrimary(state: WizardState, running = false): { kind: PrimaryKind; label_key: string } {
  if (running) return { kind: "continue", label_key: "setup.overview.continue" };
  switch (state) {
    case "interrupted":
      return { kind: "resume", label_key: "setup.overview.continue" };
    case "running":
      return { kind: "continue", label_key: "setup.overview.continue" };
    case "not_started":
      return { kind: "start", label_key: "setup.overview.start" };
    case "done":
    case "failed":
    case "aborted":
      return { kind: "run_again", label_key: "setup.overview.run_again" };
  }
}

/** One result entry as summary text: an enum's option label, a
 *  `setup.summary.<key>` template, else null (left out). */
function summaryPart(key: string, value: unknown): string | null {
  if (typeof value !== "string" && typeof value !== "number" && typeof value !== "boolean") return null;
  if (hasSetupText(`setup.input.${key}.${String(value)}`)) return optionText(`setup.input.${key}`, value, null);
  if (hasSetupText(`setup.summary.${key}`)) return setupText(`setup.summary.${key}`, { v: formatValue(value) });
  return null;
}

/** A card's one-line summary: the flight controller's flag when it has one,
 *  "Not needed" when skipped, else up to three facts from its finished
 *  steps' results; empty when there are none. */
export function cardSummary(card: SetupCard): string {
  if (card.flag) return card.flag;
  if (card.state === "not_needed") return setupText("setup.card.state.not_needed");
  const parts: string[] = [];
  for (const s of card.steps) {
    if (s.state !== "done" || !s.result) continue;
    for (const [k, v] of Object.entries(s.result)) {
      const p = summaryPart(k, v);
      if (p) parts.push(p);
    }
  }
  return parts.slice(0, 3).join(" · ");
}

/** Result keys the engine stores for itself, never shown to the operator. */
const INTERNAL_RESULT_KEYS: Readonly<Record<string, true>> = { uid: true, uid_source: true, mission_state_inferred: true };

/** A snapshot name or stamp (`2026-11-24T13-09-07Z.param`, or an ISO time)
 *  as a local date and time; null when it is neither. */
export function snapshotTime(stamp: string): string | null {
  const m = /^(\d{4}-\d{2}-\d{2})T(\d{2})[-:](\d{2})[-:](\d{2})Z/.exec(stamp);
  const d = new Date(m ? `${m[1]}T${m[2]}:${m[3]}:${m[4]}Z` : stamp);
  return Number.isNaN(d.getTime()) ? null : d.toLocaleString();
}

/** A step result as the rows the operator reads: internal keys left out,
 *  the firmware named as its project writes it, a snapshot as its time. */
export function resultRows(result: Record<string, unknown> | null): Array<[string, unknown]> {
  if (!result) return [];
  return Object.entries(result)
    .filter(([k]) => !INTERNAL_RESULT_KEYS[k])
    .map(([k, v]): [string, unknown] => {
      if (k === "firmware" && typeof v === "string") return [k, firmwareLabel(v)];
      if (k === "snapshot" && typeof v === "string") return [k, snapshotTime(v) ?? v];
      return [k, v];
    });
}
