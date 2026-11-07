// The skill system's shared rules, DOM-free so each is unit-tested:
//   - echo: an activation write is pending until the plugin's state echoes
//     it, and reverts with "<skill> did not respond" after 3 s;
//   - preemption: one motion skill at a time; GO on a second one first writes
//     the running one's activation false, waits for its echo (or 1 s), then
//     activates the new one;
//   - bindings: the keyboard chords of the built-ins (from the spec) and of the
//     plugin skills' `default_binding`, where a later skill whose key is taken
//     gets no key and the collision is listed.

import { BUILTIN_SKILLS, skillActionId, type PendingWrite, type ReportedSkillState, type Skill } from "@/lib/skills";

/** A write with no echo after this reverts with a toast. */
export const ECHO_TIMEOUT_MS = 3000;
/** Preemption waits this long for the stopped skill's echo. */
export const PREEMPT_WAIT_MS = 1000;

export type EchoOutcome = "pending" | "confirmed" | "expired";

/** Where an activation write stands against the plugin's reported state. */
export function echoOutcome(write: PendingWrite, reported: ReportedSkillState | undefined, now: number): EchoOutcome {
  const active = reported?.state === "active";
  // Only a state received after the write can confirm it.
  if (reported?.at !== undefined && reported.at >= write.at && active === write.want) return "confirmed";
  if (now - write.at > ECHO_TIMEOUT_MS) return "expired";
  return "pending";
}

export interface PreemptDeps {
  /** Write a skill's activation key. */
  write: (skill: Skill, value: boolean) => Promise<void>;
  /** Resolve once the skill reports not active, or after `timeoutMs`. */
  waitIdle: (skill: Skill, timeoutMs: number) => Promise<void>;
}

/** Activate a motion skill after stopping every other running motion skill,
 *  in order: each stop is written and echoed (or timed out) before the next
 *  activation write. */
export async function activateExclusive(next: Skill, running: readonly Skill[], deps: PreemptDeps): Promise<void> {
  for (const other of running) {
    if (other.id === next.id) continue;
    await deps.write(other, false);
    await deps.waitIdle(other, PREEMPT_WAIT_MS);
  }
  await deps.write(next, true);
}

/** The motion skills reported active or with an activation write pending. */
export function runningMotionSkills(
  skills: readonly Skill[],
  reported: Readonly<Record<string, ReportedSkillState | undefined>>,
  pending: Readonly<Record<string, PendingWrite | undefined>>,
): Skill[] {
  return skills.filter((s) => {
    if (!s.extension?.motion || !s.extension.toggle) return false;
    const p = pending[s.id];
    return p ? p.want : reported[s.id]?.state === "active";
  });
}

// ---- keyboard bindings -----------------------------------------------------

/** Keys the cockpit itself owns; a plugin skill never takes them. */
export const RESERVED_KEYS: Readonly<Record<string, string>> = {
  o: "Layer level",
  v: "Next view",
  "[": "Previous target",
  "]": "Next target",
  enter: "Select",
  escape: "Back",
  "`": "Stream",
  f1: "Profile: Cine",
  f2: "Profile: Normal",
  f3: "Profile: Sport",
  ...Object.fromEntries(Array.from({ length: 9 }, (_, i) => [String(i + 1), "Stream"])),
};

const MODIFIERS = ["ctrl", "alt", "meta", "shift"] as const;

/** A chord in canonical form: lower-case, modifiers in a fixed order
 *  (`Shift+A` → `shift+a`, ` ` → `space`). Null when empty. */
export function normaliseChord(chord: string): string | null {
  const parts = chord
    .toLowerCase()
    .split("+")
    .map((p) => p.trim())
    .filter((p) => p !== "");
  // `shift++` style chords keep their literal `+`.
  if (chord.endsWith("++")) parts.push("+");
  const key = parts.filter((p) => !(MODIFIERS as readonly string[]).includes(p)).pop();
  if (!key) return null;
  const mods = MODIFIERS.filter((m) => parts.includes(m));
  return [...mods, key === " " ? "space" : key].join("+");
}

/** The chord a keydown event spells, in canonical form. Shift is folded into
 *  printable symbols (`shift+]` arrives as `}`), so it counts only for
 *  letters, digits and named keys. */
export function chordFromEvent(e: { key: string; shiftKey: boolean; ctrlKey: boolean; altKey: boolean; metaKey: boolean }): string | null {
  const raw = e.key === " " ? "space" : e.key.toLowerCase();
  const named = raw.length > 1 || /^[a-z0-9]$/.test(raw);
  const mods = [e.ctrlKey && "ctrl", e.altKey && "alt", e.metaKey && "meta", e.shiftKey && named && "shift"].filter(
    (m): m is string => typeof m === "string",
  );
  return normaliseChord([...mods, raw].join("+"));
}

export interface BindingConflict {
  key: string;
  /** The action that got no key. */
  actionId: string;
  label: string;
  /** Who holds the key. */
  heldBy: string;
  heldByLabel: string;
}

export interface BindingTable {
  /** Canonical chord → cockpit action id. */
  keys: Record<string, string>;
  conflicts: BindingConflict[];
}

/** The keyboard map: reserved keys first, then the built-ins in spec order,
 *  then the plugin skills in load order; a later skill whose key is taken
 *  gets none and is listed. */
export function buildBindings(pluginSkills: readonly Skill[], builtins: readonly Skill[] = BUILTIN_SKILLS): BindingTable {
  const keys: Record<string, string> = {};
  const labels: Record<string, string> = {};
  const conflicts: BindingConflict[] = [];
  const claim = (chord: string | null | undefined, actionId: string, label: string) => {
    const key = chord ? normaliseChord(chord) : null;
    if (!key) return;
    const reserved = RESERVED_KEYS[key];
    const holder = keys[key];
    if (reserved !== undefined || holder !== undefined) {
      conflicts.push({
        key,
        actionId,
        label,
        heldBy: holder ?? `reserved:${key}`,
        heldByLabel: holder !== undefined ? labels[holder] : reserved!,
      });
      return;
    }
    keys[key] = actionId;
    labels[actionId] = label;
  };
  for (const b of builtins) claim(b.builtin?.keyboard, skillActionId(b), b.label);
  for (const s of pluginSkills) claim(s.extension?.defaultBinding?.key, skillActionId(s), s.label);
  return { keys, conflicts };
}
