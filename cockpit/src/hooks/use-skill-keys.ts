// The skills as cockpit actions, and their keyboard chords, while the Feed is
// mounted. Every built-in registers under its spec id (`arm`, `rth`, `kill`,
// …) and every installed plugin skill as `skill:<pluginId>:<skillId>`, so a
// controller binding names an action and fires through `runSkillAction`, the
// same gate and confirm tier as a tap. Keyboard chords come from the spec's
// built-in keys and the plugin skills' `default_binding` (collisions resolved
// in the extensions store); a held chord holds the confirm sheet, and its
// key-up ends the hold.

import { useEffect } from "react";

import { skillContextNow } from "@/hooks/use-skill-context";
import { registerCockpitActions, type CockpitAction } from "@/lib/cockpit-actions";
import { chordFromEvent } from "@/lib/skill-gates";
import { runSkillAction } from "@/lib/skill-runner";
import { BUILTIN_SKILLS, skillActionId, type Skill } from "@/lib/skills";
import { useConfirmStore } from "@/stores/confirm-store";
import { pluginSkills, useExtensionsStore } from "@/stores/extensions-store";

/** The skill a cockpit action id names, or null. */
export function skillForAction(actionId: string): Skill | null {
  return [...BUILTIN_SKILLS, ...pluginSkills()].find((s) => skillActionId(s) === actionId) ?? null;
}

function typingTarget(t: EventTarget | null, key: string): boolean {
  if (!(t instanceof HTMLElement)) return false;
  if (t.isContentEditable || t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.tagName === "SELECT") return true;
  // Space on a focused button presses that button.
  return key === " " && t.tagName === "BUTTON";
}

export function useSkillKeys(): void {
  const extensions = useExtensionsStore((s) => s.extensions);

  useEffect(() => {
    const actions: CockpitAction[] = [...BUILTIN_SKILLS, ...extensions.flatMap((e) => e.skills)].map((skill) => ({
      id: skillActionId(skill),
      label: skill.extension ? `${skill.extension.pluginName}: ${skill.label}` : skill.label,
      group: skill.builtin ? "flight" : skill.category === "camera" ? "camera" : "skills",
      run: (phase) => runSkillAction(skill, phase, skillContextNow()),
    }));
    return registerCockpitActions(actions);
  }, [extensions]);

  useEffect(() => {
    // The chord's key that is down, and the skill it holds.
    let held: { key: string; skill: Skill } | null = null;

    const onDown = (e: KeyboardEvent) => {
      if (e.repeat || typingTarget(e.target, e.key)) return;
      const chord = chordFromEvent(e);
      const actionId = chord ? useExtensionsStore.getState().keys[chord] : undefined;
      const skill = actionId ? skillForAction(actionId) : null;
      if (!skill) return;
      const open = useConfirmStore.getState().pending;
      if (open && open.skill.id !== skill.id) return;
      e.preventDefault();
      held = { key: e.key.toLowerCase(), skill };
      runSkillAction(skill, "press", skillContextNow(), true);
    };
    const onUp = (e: KeyboardEvent) => {
      if (!held || e.key.toLowerCase() !== held.key) return;
      runSkillAction(held.skill, "release", skillContextNow());
      held = null;
    };

    window.addEventListener("keydown", onDown);
    window.addEventListener("keyup", onUp);
    return () => {
      window.removeEventListener("keydown", onDown);
      window.removeEventListener("keyup", onUp);
    };
  }, []);
}
