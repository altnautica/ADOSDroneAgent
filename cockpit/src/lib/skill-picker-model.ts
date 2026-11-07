// The skill picker's pure model: its tabs, the per-plugin grouping, the
// Tweak panel's visible parameters and the D-pad step of one parameter.

import type { ParamValue, PluginSkillCategory, Skill, SkillParam } from "@/lib/skills";

export const PICKER_TABS: readonly { id: PluginSkillCategory; label: string }[] = [
  { id: "behavior", label: "Behaviors" },
  { id: "camera", label: "Camera" },
  { id: "navigation", label: "Navigation" },
  { id: "utility", label: "Utility" },
];

/** Skills of one tab grouped under their plugin's name, in install order. */
export function groupByPlugin(skills: readonly Skill[]): { plugin: string; skills: Skill[] }[] {
  const out: { plugin: string; skills: Skill[] }[] = [];
  for (const s of skills) {
    const name = s.extension?.pluginName ?? "";
    const g = out.find((x) => x.plugin === name);
    if (g) g.skills.push(s);
    else out.push({ plugin: name, skills: [s] });
  }
  return out;
}

/** The parameters of a skill visible for the current values (`visible_if`). */
export function visibleParams(params: readonly SkillParam[], values: Readonly<Record<string, ParamValue | null>>): SkillParam[] {
  return params.filter((p) => !p.visibleIf || values[p.visibleIf.key] === p.visibleIf.equals);
}

/** One step of a parameter: a number moves by its step within its range, an
 *  enum to the next/previous value, a boolean flips. */
export function stepParam(p: SkillParam, value: ParamValue | null, dir: 1 | -1): ParamValue {
  if (p.kind === "number") {
    const v = typeof value === "number" ? value : (p.defaultValue ?? p.min);
    return Math.min(p.max, Math.max(p.min, Math.round((v + dir * p.step) / p.step) * p.step));
  }
  if (p.kind === "enum") {
    const i = Math.max(0, p.values.indexOf(String(value ?? p.defaultValue ?? p.values[0])));
    return p.values[(i + dir + p.values.length) % p.values.length];
  }
  return !(value ?? p.defaultValue ?? false);
}

/** Each parameter's current value: the plugin's echo, else its configured
 *  value, else the manifest default. */
export function paramValues(
  skill: Skill,
  echo: Readonly<Record<string, ParamValue>> | undefined,
  config: Readonly<Record<string, ParamValue>> | undefined,
): Record<string, ParamValue | null> {
  const out: Record<string, ParamValue | null> = {};
  for (const p of skill.params ?? []) out[p.key] = echo?.[p.key] ?? config?.[p.key] ?? p.defaultValue;
  return out;
}
