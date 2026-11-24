// The Aircraft setup overview: the wizard's steps grouped into component
// cards (Frame, Sensors, Radio, ...), each card's state and one-line summary,
// the progress count, and which primary button the overview offers. Pure
// functions over the wire types, so every surface derives the same thing.

import type { TFunction } from "i18next";

import { formatValue, hasSetupText, optionText, setupText } from "./strings";
import type { SetupStep, Wizard, WizardState } from "./types";

/** The vehicle class in words (`setupWizard.vehicleClass.*`), else as sent. */
export function vehicleClassLabel(t: TFunction, cls: string | null): string | null {
  return cls === "multirotor" || cls === "fixedwing" || cls === "vtol" ? t(`setupWizard.vehicleClass.${cls}`) : cls;
}

export type CardState = "done" | "attention" | "in_progress" | "not_set" | "optional" | "not_needed";

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
}

/** A step the wizard has moved past: done, or skipped as not needed. */
function finished(s: SetupStep): boolean {
  return s.state === "done" || s.state === "skipped";
}

/** A card's state from its steps: a failed or blocked step needs attention;
 *  an active step is in progress; all skipped is not needed; all finished is
 *  done; otherwise not set (or optional). */
export function cardState(steps: readonly SetupStep[], optional = false): CardState {
  if (steps.some((s) => s.state === "failed" || (s.blockers.length > 0 && !finished(s)))) return "attention";
  if (steps.some((s) => s.state === "running" || s.state === "waiting_user")) return "in_progress";
  if (steps.length > 0 && steps.every((s) => s.state === "skipped")) return "not_needed";
  if (steps.length > 0 && steps.every(finished)) return "done";
  return optional ? "optional" : "not_set";
}

/** The wizard's cards: the ground wizard by component, any other wizard one
 *  card per step. */
export function wizardCards(wizard: Wizard): SetupCard[] {
  if (wizard.id !== "ground") {
    return wizard.steps.map((s) => ({ id: s.step, title_key: s.title_key, steps: [s], state: cardState([s]) }));
  }
  const byId = new Map(wizard.steps.map((s) => [s.step, s]));
  const cards: SetupCard[] = [];
  for (const def of GROUND_CARDS) {
    const steps = def.steps.map((id) => byId.get(id)).filter((s): s is SetupStep => s !== undefined);
    if (steps.length === 0) continue;
    cards.push({ id: def.id, title_key: `setup.card.${def.id}.title`, steps, state: cardState(steps, def.optional) });
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

/** The step a card opens: its first unfinished step, else its first. */
export function cardTarget(card: SetupCard): SetupStep {
  return card.steps.find((s) => !finished(s)) ?? card.steps[0];
}

export type PrimaryKind = "start" | "resume" | "continue" | "run_again";

/** The overview's primary button: Continue (resume an interrupted run, or open
 *  the running one), Start, or Run again. */
export function overviewPrimary(state: WizardState): { kind: PrimaryKind; label_key: string } {
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

/** A card's one-line summary: "Not needed" when skipped, else up to three
 *  facts from its finished steps' results; empty when there are none. */
export function cardSummary(card: SetupCard): string {
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
